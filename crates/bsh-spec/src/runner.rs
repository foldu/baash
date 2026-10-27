//! Case execution, comparison, and aggregation.

use crate::diff;
use crate::json::Json;
use crate::parse::{Case, Suite, TestFile};

use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// Cap for one stream; beyond this the case is failed as "output too large".
const OUTPUT_CAP: usize = 8 * 1024 * 1024;

/// A reference shell to run alongside the shell under test.
#[derive(Debug, Clone)]
pub struct Reference {
    pub label: String,
    pub path: PathBuf,
}

/// One parsed file after `--filter` selection.
#[derive(Debug, Clone)]
pub struct FilePlan {
    pub file: TestFile,
    /// Indices into `file.cases` that the filter selected.
    pub selected: Vec<usize>,
}

/// Run configuration.
#[derive(Debug, Clone)]
pub struct Options {
    pub shell: PathBuf,
    pub spec_dir: PathBuf,
    /// Directory holding the suite's helper programs (`argv.py`, …).
    pub helpers: PathBuf,
    pub timeout: Duration,
    pub jobs: usize,
    pub keep_temp: bool,
    pub compare: Vec<Reference>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutcomeKind {
    Pass,
    Fail,
    Skip,
    Timeout,
}

impl OutcomeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Fail => "fail",
            Self::Skip => "skip",
            Self::Timeout => "timeout",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ReferenceResult {
    pub label: String,
    pub kind: OutcomeKind,
    pub detail: String,
}

#[derive(Debug, Clone)]
pub struct CaseResult {
    pub name: String,
    pub line: usize,
    pub kind: OutcomeKind,
    /// Short reason: skip reason, or the primary mismatch.
    pub reason: Option<String>,
    /// Full detail, including a truncated diff for failures.
    pub detail: String,
    /// A pass that relied on a per-shell `## OK ... STDOUT:` alternative.
    pub used_ok_override: bool,
    pub duration: Duration,
    pub references: Vec<ReferenceResult>,
}

#[derive(Debug, Clone)]
pub struct FileResult {
    pub name: String,
    pub cases: Vec<CaseResult>,
    pub duration: Duration,
}

impl FileResult {
    pub fn passed(&self) -> usize {
        self.count(OutcomeKind::Pass)
    }
    pub fn compared(&self) -> usize {
        self.cases
            .iter()
            .filter(|case| case.kind != OutcomeKind::Skip)
            .count()
    }
    pub fn skipped(&self) -> usize {
        self.count(OutcomeKind::Skip)
    }
    pub fn failed(&self) -> usize {
        self.count(OutcomeKind::Fail)
    }
    pub fn timeouts(&self) -> usize {
        self.count(OutcomeKind::Timeout)
    }
    fn count(&self, kind: OutcomeKind) -> usize {
        self.cases.iter().filter(|case| case.kind == kind).count()
    }
}

#[derive(Debug, Clone)]
pub struct CompareSummary {
    pub label: String,
    pub path: PathBuf,
    pub compared: usize,
    pub passed: usize,
    /// Ours failed/timed out while the reference passed.
    pub we_regress: usize,
    /// Ours passed while the reference failed/timed out.
    pub we_improve: usize,
}

#[derive(Debug, Clone)]
pub struct Report {
    pub shell: PathBuf,
    pub spec_dir: PathBuf,
    pub files: Vec<FileResult>,
    pub skip_reasons: BTreeMap<String, usize>,
    pub compare: Vec<CompareSummary>,
    pub duration: Duration,
}

impl Report {
    pub fn all(&self) -> usize {
        self.files.iter().map(|f| f.cases.len()).sum()
    }
    pub fn compared(&self) -> usize {
        self.files.iter().map(FileResult::compared).sum()
    }
    pub fn passed(&self) -> usize {
        self.files.iter().map(FileResult::passed).sum()
    }
    /// Passes counting only a case's primary expectation.
    ///
    /// A case that passed by matching an `## OK <shell>` alternative is not a
    /// strict pass: an upstream runner given a shell name that appears in no
    /// block would have judged it against the primary expectation alone.
    pub fn passed_strict(&self) -> usize {
        self.passed() - self.ok_overrides()
    }
    pub fn skipped(&self) -> usize {
        self.files.iter().map(FileResult::skipped).sum()
    }
    pub fn failed(&self) -> usize {
        self.files.iter().map(FileResult::failed).sum()
    }
    pub fn timeouts(&self) -> usize {
        self.files.iter().map(FileResult::timeouts).sum()
    }
    pub fn ok_overrides(&self) -> usize {
        self.files
            .iter()
            .flat_map(|file| &file.cases)
            .filter(|case| case.used_ok_override)
            .count()
    }

    pub fn json(&self) -> Json {
        let totals = Json::object([
            ("passed", Json::Number(self.passed() as u64)),
            ("compared", Json::Number(self.compared() as u64)),
            ("skipped", Json::Number(self.skipped() as u64)),
            ("failed", Json::Number(self.failed() as u64)),
            ("timeouts", Json::Number(self.timeouts() as u64)),
            (
                "ok_override_passes",
                Json::Number(self.ok_overrides() as u64),
            ),
            ("all", Json::Number(self.all() as u64)),
        ]);
        let skips = Json::object(
            self.skip_reasons
                .iter()
                .map(|(reason, count)| (reason.clone(), Json::Number(*count as u64))),
        );
        let compare: Vec<Json> = self
            .compare
            .iter()
            .map(|summary| {
                Json::object([
                    ("shell", Json::string(&summary.label)),
                    ("path", Json::string(summary.path.display().to_string())),
                    ("compared", Json::Number(summary.compared as u64)),
                    ("passed", Json::Number(summary.passed as u64)),
                    ("we_regress", Json::Number(summary.we_regress as u64)),
                    ("we_improve", Json::Number(summary.we_improve as u64)),
                ])
            })
            .collect();
        let files: Vec<Json> = self
            .files
            .iter()
            .map(|file| {
                let cases: Vec<Json> = file
                    .cases
                    .iter()
                    .map(|case| {
                        let references: Vec<Json> = case
                            .references
                            .iter()
                            .map(|reference| {
                                Json::object([
                                    ("shell", Json::string(&reference.label)),
                                    ("outcome", Json::string(reference.kind.as_str())),
                                    ("detail", Json::string(clip(&reference.detail, 4096))),
                                ])
                            })
                            .collect();
                        Json::object([
                            ("name", Json::string(&case.name)),
                            ("line", Json::Number(case.line as u64)),
                            ("outcome", Json::string(case.kind.as_str())),
                            (
                                "reason",
                                match &case.reason {
                                    Some(reason) => Json::string(reason),
                                    None => Json::Null,
                                },
                            ),
                            ("detail", Json::string(clip(&case.detail, 8192))),
                            ("used_ok_override", Json::Bool(case.used_ok_override)),
                            ("references", Json::array(references)),
                        ])
                    })
                    .collect();
                Json::object([
                    ("file", Json::string(&file.name)),
                    ("passed", Json::Number(file.passed() as u64)),
                    ("compared", Json::Number(file.compared() as u64)),
                    ("skipped", Json::Number(file.skipped() as u64)),
                    ("failed", Json::Number(file.failed() as u64)),
                    ("timeouts", Json::Number(file.timeouts() as u64)),
                    (
                        "duration_ms",
                        Json::Number(file.duration.as_millis() as u64),
                    ),
                    ("cases", Json::array(cases)),
                ])
            })
            .collect();
        Json::object([
            ("shell", Json::string(self.shell.display().to_string())),
            (
                "spec_dir",
                Json::string(self.spec_dir.display().to_string()),
            ),
            (
                "duration_ms",
                Json::Number(self.duration.as_millis() as u64),
            ),
            ("totals", totals),
            ("skip_reasons", skips),
            ("compare", Json::array(compare)),
            ("files", Json::array(files)),
        ])
    }
}

/// Run every selected case in every file.
pub fn run(plans: &[FilePlan], options: &Options) -> Result<Report, String> {
    let started = Instant::now();
    let shim = RepoShim::new(&options.spec_dir)?;
    let bin_dir = &options.helpers;
    // The vendored helpers in spec/oils/bin all start with
    // `#!/usr/bin/env python2`.  When python2 is unavailable, use Python 3
    // equivalents (same output) instead of failing every argv.py test for an
    // environmental reason.
    let shim_bin = if command_on_path("python2") {
        None
    } else {
        Some(shim.write_helper_shims()?)
    };
    let mut path = String::new();
    if let Some(dir) = &shim_bin {
        path.push_str(&dir.display().to_string());
        path.push(':');
    }
    if bin_dir.is_dir() {
        path.push_str(&bin_dir.display().to_string());
        path.push(':');
    }
    if let Some(inherited) = std::env::var_os("PATH") {
        path.push_str(&inherited.to_string_lossy());
    } else {
        path.push_str("/usr/local/bin:/usr/bin:/bin");
    }

    let results: Mutex<Vec<Option<FileResult>>> =
        Mutex::new((0..plans.len()).map(|_| None).collect());
    let next = AtomicUsize::new(0);
    let case_counter = AtomicUsize::new(0);
    let jobs = options.jobs.max(1).min(plans.len().max(1));

    std::thread::scope(|scope| {
        for _ in 0..jobs {
            scope.spawn(|| {
                loop {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    if index >= plans.len() {
                        break;
                    }
                    let result = run_file(&plans[index], options, &shim, &path, &case_counter);
                    results.lock().expect("result mutex")[index] = Some(result);
                }
            });
        }
    });

    let files: Vec<FileResult> = results
        .into_inner()
        .expect("result mutex")
        .into_iter()
        .map(|result| result.expect("every file has a result"))
        .collect();

    let mut skip_reasons = BTreeMap::new();
    for file in &files {
        for case in &file.cases {
            if let Some(reason) = &case.reason {
                if case.kind == OutcomeKind::Skip {
                    *skip_reasons.entry(reason.clone()).or_insert(0) += 1;
                }
            }
        }
    }

    let mut compare = Vec::new();
    for reference in &options.compare {
        let mut compared = 0;
        let mut passed = 0;
        let mut we_regress = 0;
        let mut we_improve = 0;
        for file in &files {
            for case in &file.cases {
                if case.kind == OutcomeKind::Skip {
                    continue;
                }
                let Some(result) = case
                    .references
                    .iter()
                    .find(|result| result.label == reference.label)
                else {
                    continue;
                };
                compared += 1;
                if result.kind == OutcomeKind::Pass {
                    passed += 1;
                }
                let ours_passed = case.kind == OutcomeKind::Pass;
                if !ours_passed && result.kind == OutcomeKind::Pass {
                    we_regress += 1;
                }
                if ours_passed && result.kind != OutcomeKind::Pass {
                    we_improve += 1;
                }
            }
        }
        compare.push(CompareSummary {
            label: reference.label.clone(),
            path: reference.path.clone(),
            compared,
            passed,
            we_regress,
            we_improve,
        });
    }

    if !options.keep_temp {
        fs::remove_dir_all(&shim.root).ok();
    } else {
        eprintln!("bsh-spec: kept temp directory {}", shim.root.display());
    }

    Ok(Report {
        shell: options.shell.clone(),
        spec_dir: options.spec_dir.clone(),
        files,
        skip_reasons,
        compare,
        duration: started.elapsed(),
    })
}

fn run_file(
    plan: &FilePlan,
    options: &Options,
    shim: &RepoShim,
    path: &str,
    case_counter: &AtomicUsize,
) -> FileResult {
    let started = Instant::now();
    let mut cases = Vec::with_capacity(plan.selected.len());
    for &index in &plan.selected {
        let case = &plan.file.cases[index];
        cases.push(run_case(
            &plan.file,
            case,
            options,
            shim,
            path,
            case_counter,
        ));
    }
    FileResult {
        name: plan.file.name.clone(),
        cases,
        duration: started.elapsed(),
    }
}

/// Skip a case whose script invokes a helper this environment does not
/// provide.  ßhell cannot be blamed for a test fixture that needs `python2`.
fn missing_helper(case: &Case) -> Option<String> {
    if !command_on_path("python2") && invokes_command(&case.code, "python2") {
        return Some("missing helper: python2".to_string());
    }
    None
}

/// Check that the suite's helper programs can actually run.
///
/// Every helper in `spec/oils/bin` is a `python2` script. When `python2` is
/// absent the harness writes Python 3 equivalents, so `python3` is required
/// instead. If neither exists, `argv.py` fails in 772 lines across 58 files
/// *for every shell* — which would look like the shell regressing when in fact
/// nothing could be measured. Callers turn this into a distinct exit code so
/// "cannot measure" never looks like "got worse".
pub fn require_helper_runtime() -> Result<(), String> {
    if command_on_path("python2") || command_on_path("python3") {
        return Ok(());
    }
    Err("the suite's helpers need python2 or python3 on PATH; \
         without one, no case can be measured"
        .to_string())
}

fn command_on_path(name: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| dir.join(name).is_file())
}

fn invokes_command(code: &str, name: &str) -> bool {
    fn starts_with_word(text: &str, word: &str) -> bool {
        let Some(rest) = text.strip_prefix(word) else {
            return false;
        };
        rest.is_empty()
            || rest.starts_with(|c: char| c.is_whitespace() || matches!(c, ';' | '|' | '&' | ')'))
    }

    for line in code.lines() {
        let trimmed = line.trim_start();
        if starts_with_word(trimmed, name) {
            return true;
        }
        for marker in ["$(", "`", "|", "&&", "||", ";"] {
            if let Some(index) = line.find(marker) {
                let rest = line[index + marker.len()..].trim_start();
                if starts_with_word(rest, name) {
                    return true;
                }
            }
        }
    }
    false
}

fn skip_reason(file: &TestFile, case: &Case) -> Option<String> {
    if file.meta.get("our_shell").map(String::as_str) == Some("ysh")
        || case.our_shell() == Some("ysh")
    {
        return Some("our_shell: ysh".to_string());
    }
    if file.suite == Suite::Ysh {
        return Some("ysh suite (out of scope)".to_string());
    }
    if file.suite == Suite::Tea {
        return Some("tea suite (out of scope)".to_string());
    }
    None
}

fn run_case(
    file: &TestFile,
    case: &Case,
    options: &Options,
    shim: &RepoShim,
    path: &str,
    case_counter: &AtomicUsize,
) -> CaseResult {
    let started = Instant::now();
    if let Some(reason) = skip_reason(file, case).or_else(|| missing_helper(case)) {
        return CaseResult {
            name: case.name.clone(),
            line: case.line,
            kind: OutcomeKind::Skip,
            reason: Some(reason),
            detail: String::new(),
            used_ok_override: false,
            duration: Duration::ZERO,
            references: Vec::new(),
        };
    }

    let id = case_counter.fetch_add(1, Ordering::Relaxed);
    let primary = run_one(
        &options.shell,
        "bsh",
        case,
        options,
        shim,
        path,
        id,
        file.legacy_tmp_dir(),
    );
    let verdict = judge(case, &primary, "bsh", true, options.timeout);

    let mut references = Vec::new();
    for reference in &options.compare {
        let actual = run_one(
            &reference.path,
            &reference.label,
            case,
            options,
            shim,
            path,
            id,
            file.legacy_tmp_dir(),
        );
        let reference_verdict = judge(case, &actual, &reference.label, false, options.timeout);
        references.push(ReferenceResult {
            label: reference.label.clone(),
            kind: reference_verdict.kind,
            detail: reference_verdict.detail,
        });
    }

    CaseResult {
        name: case.name.clone(),
        line: case.line,
        kind: verdict.kind,
        reason: verdict.reason,
        detail: verdict.detail,
        used_ok_override: verdict.used_ok_override,
        duration: started.elapsed(),
        references,
    }
}

struct Verdict {
    kind: OutcomeKind,
    reason: Option<String>,
    detail: String,
    used_ok_override: bool,
}

fn judge(case: &Case, actual: &Actual, label: &str, primary: bool, timeout: Duration) -> Verdict {
    if actual.timed_out {
        return Verdict {
            kind: OutcomeKind::Timeout,
            reason: Some(format!("timeout after {:.1}s", timeout.as_secs_f64())),
            used_ok_override: false,
            detail: format!(
                "timed out after {:.1}s (status {}, {} bytes stdout, {} bytes stderr)",
                timeout.as_secs_f64(),
                actual.status,
                actual.stdout.len(),
                actual.stderr.len()
            ),
        };
    }
    if let Some(error) = &actual.spawn_error {
        return Verdict {
            kind: OutcomeKind::Fail,
            reason: Some("could not execute shell".to_string()),
            used_ok_override: false,
            detail: error.clone(),
        };
    }
    if actual.truncated {
        return Verdict {
            kind: OutcomeKind::Fail,
            reason: Some("output exceeded harness cap".to_string()),
            used_ok_override: false,
            detail: format!(
                "stdout/stderr exceeded the {OUTPUT_CAP} byte cap; comparison cannot be exact"
            ),
        };
    }

    let mut messages = Vec::new();
    let mut details = Vec::new();
    let mut used_ok_override = false;

    let alternatives = case.stdout_for(label, primary);
    let matched_alternative = alternatives
        .iter()
        .any(|expected| *expected == actual.stdout);
    if primary && matched_alternative {
        let defaults = case.default_stdout();
        used_ok_override = !defaults.iter().any(|expected| *expected == actual.stdout);
    }
    if !alternatives.is_empty() && !matched_alternative {
        messages.push("stdout mismatch".to_string());
        let expected = alternatives[0];
        let mut text = String::from("stdout differs:\n");
        text.push_str(&diff::unified_diff(expected, &actual.stdout, 80));
        if alternatives.len() > 1 {
            text.push_str(&format!(
                "(no alternative matched; {} alternatives checked)\n",
                alternatives.len()
            ));
        }
        details.push(text);
    }

    if let Some(expected) = case.stderr_for(label, primary) {
        if expected != actual.stderr {
            messages.push("stderr mismatch".to_string());
            let mut text = String::from("stderr differs:\n");
            text.push_str(&diff::unified_diff(expected, &actual.stderr, 40));
            details.push(text);
        }
    }

    let expected_status = case.status_for(label, primary);
    if expected_status != actual.status {
        messages.push(format!(
            "status: expected {expected_status}, got {}",
            actual.status
        ));
    }

    if messages.is_empty() {
        Verdict {
            kind: OutcomeKind::Pass,
            reason: None,
            detail: String::new(),
            used_ok_override,
        }
    } else {
        Verdict {
            kind: OutcomeKind::Fail,
            reason: Some(messages.join("; ")),
            detail: clip(&details.join("\n"), 16 * 1024),
            used_ok_override: false,
        }
    }
}

struct Actual {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    status: i32,
    timed_out: bool,
    truncated: bool,
    spawn_error: Option<String>,
}

#[allow(clippy::too_many_arguments)]
fn run_one(
    shell: &Path,
    label: &str,
    case: &Case,
    options: &Options,
    shim: &RepoShim,
    path: &str,
    id: usize,
    legacy_tmp_dir: bool,
) -> Actual {
    let dir = shim.root.join(format!("case-{id:05}-{label}"));
    if let Err(error) = fs::create_dir_all(&dir) {
        return Actual {
            stdout: Vec::new(),
            stderr: Vec::new(),
            status: 0,
            timed_out: false,
            truncated: false,
            spawn_error: Some(format!("could not create {}: {error}", dir.display())),
        };
    }
    if legacy_tmp_dir {
        fs::create_dir_all(dir.join("_tmp")).ok();
    }

    let mut command = Command::new(shell);
    command
        .current_dir(&dir)
        .env_clear()
        .env("PATH", path)
        .env("SH", shell)
        .env("TMP", &dir)
        .env("REPO_ROOT", &shim.repo)
        .env("LC_ALL", "C")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Put the shell and everything it spawns in one process group so a
    // timeout can reap background children too.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            if !options.keep_temp {
                fs::remove_dir_all(&dir).ok();
            }
            return Actual {
                stdout: Vec::new(),
                stderr: Vec::new(),
                status: 0,
                timed_out: false,
                truncated: false,
                spawn_error: Some(format!("could not run {}: {error}", shell.display())),
            };
        }
    };

    let code = case.code.clone();
    let mut child_stdin = child.stdin.take().expect("piped stdin");
    let writer = std::thread::spawn(move || {
        use std::io::Write;
        let _ = child_stdin.write_all(code.as_bytes());
    });
    let child_stdout = child.stdout.take().expect("piped stdout");
    let child_stderr = child.stderr.take().expect("piped stderr");
    let stdout_reader = std::thread::spawn(move || read_capped(child_stdout));
    let stderr_reader = std::thread::spawn(move || read_capped(child_stderr));

    let deadline = Instant::now() + options.timeout;
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() >= deadline {
                    timed_out = true;
                    kill_group(&mut child);
                    break child.wait().expect("wait after kill");
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            Err(_) => {
                timed_out = true;
                kill_group(&mut child);
                break child.wait().expect("wait after error");
            }
        }
    };

    writer.join().ok();
    let (stdout, stdout_truncated) = stdout_reader.join().unwrap_or_default();
    let (stderr, stderr_truncated) = stderr_reader.join().unwrap_or_default();

    if !options.keep_temp {
        fs::remove_dir_all(&dir).ok();
    }

    Actual {
        stdout,
        stderr,
        status: exit_code(status),
        timed_out,
        truncated: stdout_truncated || stderr_truncated,
        spawn_error: None,
    }
}

fn read_capped(mut reader: impl Read) -> (Vec<u8>, bool) {
    let mut output = Vec::new();
    let mut truncated = false;
    let mut buffer = [0u8; 8192];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => {
                let room = OUTPUT_CAP.saturating_sub(output.len());
                if count <= room {
                    output.extend_from_slice(&buffer[..count]);
                } else {
                    output.extend_from_slice(&buffer[..room]);
                    truncated = true;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    (output, truncated)
}

fn kill_group(child: &mut std::process::Child) {
    let pid = child.id();
    // `kill` is the POSIX way to signal a process group without unsafe code.
    let _ = Command::new("kill")
        .args(["-KILL", "--", &format!("-{pid}")])
        .status();
    child.kill().ok();
}

fn exit_code(status: std::process::ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return -signal;
        }
    }
    0
}

fn clip(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut clipped = text[..end].to_string();
    clipped.push_str("\n... (truncated)\n");
    clipped
}

/// A throwaway directory that mirrors the upstream repository layout enough
/// for `$REPO_ROOT/spec/testdata/...` to resolve to the vendored tree.
struct RepoShim {
    root: PathBuf,
    repo: PathBuf,
    shim_bin: PathBuf,
}

impl RepoShim {
    fn new(spec_dir: &Path) -> Result<Self, String> {
        let spec_dir = spec_dir
            .canonicalize()
            .map_err(|error| format!("cannot resolve {}: {error}", spec_dir.display()))?;
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let root = std::env::temp_dir().join(format!("bsh-spec-{}-{stamp}", std::process::id()));
        fs::create_dir_all(&root)
            .map_err(|error| format!("cannot create {}: {error}", root.display()))?;
        let repo = root.join("repo");
        fs::create_dir_all(&repo)
            .map_err(|error| format!("cannot create {}: {error}", repo.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            symlink(&spec_dir, repo.join("spec"))
                .map_err(|error| format!("cannot link {}: {error}", repo.join("spec").display()))?;
            let testdata = spec_dir.join("testdata");
            if testdata.is_dir() {
                symlink(&testdata, repo.join("testdata")).map_err(|error| {
                    format!("cannot link {}: {error}", repo.join("testdata").display())
                })?;
            }
        }
        let shim_bin = root.join("shim-bin");
        fs::create_dir_all(&shim_bin)
            .map_err(|error| format!("cannot create {}: {error}", shim_bin.display()))?;
        Ok(Self {
            root,
            repo,
            shim_bin,
        })
    }

    /// Write Python 3 replacements for the suite's Python 2 helpers.
    fn write_helper_shims(&self) -> Result<PathBuf, String> {
        let helpers: [(&str, &str); 5] = [
            ("argv.py", ARGV_PY),
            ("printenv.py", PRINTENV_PY),
            ("read_from_fd.py", READ_FROM_FD_PY),
            ("show_fd_table.py", SHOW_FD_TABLE_PY),
            ("stdout_stderr.py", STDOUT_STDERR_PY),
        ];
        for (name, body) in helpers {
            let path = self.shim_bin.join(name);
            fs::write(&path, body)
                .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&path, fs::Permissions::from_mode(0o755))
                    .map_err(|error| format!("cannot chmod {}: {error}", path.display()))?;
            }
        }
        Ok(self.shim_bin.clone())
    }
}

// Python 2's `repr` of a byte string, which is what upstream's argv.py prints.
// UTF-8 arguments are emitted as `\xNN` escapes, exactly like Python 2.
const ARGV_PY: &str = r#"#!/usr/bin/env python3
import os
import sys

def py2_repr(data):
    quote = "'"
    if b"'" in data and b'"' not in data:
        quote = '"'
    out = [quote]
    for byte in data:
        ch = chr(byte)
        if ch == '\\':
            out.append('\\\\')
        elif ch == quote:
            out.append('\\' + quote)
        elif ch == '\t':
            out.append('\\t')
        elif ch == '\n':
            out.append('\\n')
        elif ch == '\r':
            out.append('\\r')
        elif 0x20 <= byte < 0x7f:
            out.append(ch)
        else:
            out.append('\\x%02x' % byte)
    out.append(quote)
    return ''.join(out)

print('[' + ', '.join(py2_repr(os.fsencode(arg)) for arg in sys.argv[1:]) + ']')
"#;

const PRINTENV_PY: &str = r#"#!/usr/bin/env python3
import os
import sys

for name in sys.argv[1:]:
    print(os.environ.get(name))
"#;

const READ_FROM_FD_PY: &str = r#"#!/usr/bin/env python3
import os
import sys

for arg in sys.argv[1:]:
    fd = int(arg)
    try:
        data = os.read(fd, 1024)
    except OSError as error:
        print('FATAL: Error reading from fd %d: %s' % (fd, error), file=sys.stderr)
        sys.exit(1)
    sys.stdout.buffer.write(('%d: ' % fd).encode())
    sys.stdout.buffer.write(data)
    sys.stdout.buffer.flush()
"#;

const SHOW_FD_TABLE_PY: &str = r#"#!/usr/bin/env python3
import os

directory = '/proc/self/fd/'
for fd in os.listdir(directory):
    path = os.path.join(directory, fd)
    try:
        connected_to = os.readlink(path)
    except OSError as error:
        print('%s %s' % (fd, error))
    else:
        print('%s %s' % (fd, connected_to))
"#;

const STDOUT_STDERR_PY: &str = r#"#!/usr/bin/env python3
import sys

stdout = sys.argv[1] if len(sys.argv) > 1 else 'STDOUT'
stderr = sys.argv[2] if len(sys.argv) > 2 else 'STDERR'
status = int(sys.argv[3]) if len(sys.argv) > 3 else 0

print(stdout)
print(stderr, file=sys.stderr)
sys.exit(status)
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse_text;

    fn parse_case(text: &str) -> Case {
        let file = parse_text("x.test.sh", Path::new("x.test.sh"), text).unwrap();
        file.cases.into_iter().next().unwrap()
    }

    #[test]
    fn missing_stdout_is_unchecked() {
        let case = parse_case("#### c\necho hi\n## status: 0\n");
        assert!(case.stdout_for("bsh", true).is_empty());
        let actual = Actual {
            stdout: b"anything\n".to_vec(),
            stderr: Vec::new(),
            status: 0,
            timed_out: false,
            truncated: false,
            spawn_error: None,
        };
        let verdict = judge(&case, &actual, "bsh", true, Duration::from_secs(10));
        assert_eq!(verdict.kind, OutcomeKind::Pass);
    }

    #[test]
    fn ok_block_is_alternative_for_primary() {
        let case = parse_case(
            "#### c\necho hi\n## STDOUT:\nhi\n## END\n## OK bash STDOUT:\nbye\n## END\n",
        );
        let actual = Actual {
            stdout: b"bye\n".to_vec(),
            stderr: Vec::new(),
            status: 0,
            timed_out: false,
            truncated: false,
            spawn_error: None,
        };
        let verdict = judge(&case, &actual, "bsh", true, Duration::from_secs(10));
        assert_eq!(verdict.kind, OutcomeKind::Pass);

        // A reference shell named `bash` prefers its own block.
        let verdict = judge(&case, &actual, "bash", false, Duration::from_secs(10));
        assert_eq!(verdict.kind, OutcomeKind::Pass);
    }

    #[test]
    fn reference_falls_back_to_default_status() {
        let case = parse_case("#### c\necho hi\n## status: 2\n## OK bash status: 1\n");
        assert_eq!(case.status_for("bsh", true), 2);
        assert_eq!(case.status_for("bash", false), 1);
        assert_eq!(case.status_for("dash", false), 2);
    }
}
