//! `baash-spec` — run the vendored Oils spec suite against baash.

#![forbid(unsafe_code)]

use baash_spec::parse::{self, TestFile};
use baash_spec::runner::{FilePlan, Options, OutcomeKind, Reference, Report};

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

const USAGE: &str = "\
baash-spec — run the Oils conformance suite against a shell

USAGE
    baash-spec [OPTIONS]

OPTIONS
    --shell PATH        Shell to test (default: target/debug/baash, built if missing)
    --spec-dir PATH     Spec directory (default: <workspace>/spec/oils)
    --filter SUBSTR     Only files whose name or cases whose name contain SUBSTR
    --compare LIST      Comma-separated reference shells (names or paths)
    --list-failures     Print each failing case with a short diff
    --json OUT          Write a machine-readable report to OUT
    --min N             Exit 1 when fewer than N cases pass
    --baseline PATH     Baseline file (default: crates/baash-spec/baseline.txt)
    --record            Write this run's numbers to the baseline file
    --ratchet           Exit 1 when the count falls below the baseline
    --strict            Ratchet and --min use the primary-expectation count
    --tolerance N       Allow a dip of N cases before the ratchet fails
    --locale NAME       LC_ALL for every shell (default: C)
    --timeout SECS      Per-case timeout (default: 10)
    --jobs N            Cases are run in parallel across files (default: 4)
    --keep-temp         Keep the per-run temp directory
    -h, --help          Print this help

The suite format follows Oils' test/sh_spec.py.  A case's stdout is compared
when it declares one (`## STDOUT:`, `## stdout:`, `## stdout-json:`); `## OK
<shell> STDOUT:` blocks are accepted as alternatives for the shell under test.
The exit status is compared when declared and defaults to 0.  Stderr is only
compared when the case declares it.  Cases with `## our_shell: ysh` and files
in the YSH suite are skipped (out of scope), never counted as passes.
";

fn main() -> std::process::ExitCode {
    // The reference runner starts every case under a `022` mask, so a case
    // that only prints the mask must print `0022` rather than inheriting
    // whatever the developer's terminal had (usually `077`).
    let _ = baash_sys::umask(0o022);
    let started = Instant::now();
    match run(started) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("baash-spec: {error}");
            std::process::ExitCode::from(2)
        }
    }
}

#[derive(Debug)]
struct Cli {
    baseline: Option<PathBuf>,
    record: bool,
    ratchet: bool,
    strict: bool,
    tolerance: usize,
    shell: Option<PathBuf>,
    spec_dir: Option<PathBuf>,
    filter: Option<String>,
    compare: Vec<String>,
    list_failures: bool,
    json: Option<PathBuf>,
    min: Option<usize>,
    timeout: Duration,
    jobs: usize,
    keep_temp: bool,
    locale: String,
}

fn run(started: Instant) -> Result<std::process::ExitCode, String> {
    let cli = parse_args()?;
    let workspace = workspace_root()?;
    let shell = match &cli.shell {
        Some(path) => {
            if !path.is_file() {
                return Err(format!(
                    "shell {} does not exist; build it or pass a different --shell",
                    path.display()
                ));
            }
            // Absolute, but with symlinks left alone. Absolute because every
            // case runs in its own temporary directory, so `--shell
            // target/debug/baash` would otherwise be looked up relative to
            // that directory and fail. Unresolved because busybox and toybox
            // dispatch on `argv[0]`: `/bin/sh` (a symlink to `/bin/busybox`)
            // resolved to `busybox` prints the multi-call binary's usage
            // instead of running a shell.
            std::path::absolute(path)
                .map_err(|error| format!("cannot resolve shell {}: {error}", path.display()))?
        }
        None => default_shell(&workspace)?,
    };
    let spec_dir = match &cli.spec_dir {
        Some(path) => path.clone(),
        None => workspace.join("spec/oils"),
    };
    if !spec_dir.is_dir() {
        return Err(format!(
            "spec directory {} does not exist (pass --spec-dir)",
            spec_dir.display()
        ));
    }
    let helpers = if spec_dir.join("bin").is_dir() {
        spec_dir.join("bin")
    } else {
        workspace.join("spec/oils/bin")
    };
    let timeout = cli.timeout;
    let references = resolve_references(&cli.compare)?;

    let mut plans = Vec::new();
    let mut errors = Vec::new();
    let entries = std::fs::read_dir(&spec_dir)
        .map_err(|error| format!("cannot read {}: {error}", spec_dir.display()))?;
    let mut names: Vec<String> = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| error.to_string())?;
        let name = entry.file_name().to_string_lossy().to_string();
        if name.ends_with(".test.sh") {
            names.push(name);
        }
    }
    names.sort();

    for name in names {
        let path = spec_dir.join(&name);
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) => {
                errors.push(format!("{}: {error}", path.display()));
                continue;
            }
        };
        match parse::parse_bytes(&name, &path, &bytes) {
            Ok(file) => {
                if let Some(plan) = select(&file, cli.filter.as_deref()) {
                    plans.push(plan);
                }
            }
            Err(error) => errors.push(format!("{}:{error}", path.display())),
        }
    }
    if !errors.is_empty() {
        for error in &errors {
            eprintln!("PARSE ERROR {error}");
        }
        return Err(format!(
            "{} spec file(s) could not be parsed; the harness cannot run",
            errors.len()
        ));
    }

    if plans.is_empty() {
        return match &cli.filter {
            Some(filter) => Err(format!("filter {filter:?} matched no cases")),
            None => Err(format!(
                "no *.test.sh files found under {}",
                spec_dir.display()
            )),
        };
    }

    let options = Options {
        shell: shell.clone(),
        spec_dir: spec_dir.clone(),
        helpers,
        timeout,
        jobs: cli.jobs,
        keep_temp: cli.keep_temp,
        compare: references,
        locale: cli.locale.clone(),
    };
    // Refuse to measure rather than report a number that means nothing.
    baash_spec::runner::require_helper_runtime()?;

    let report = baash_spec::runner::run(&plans, &options)?;

    print_report(&report, cli.list_failures);

    if let Some(path) = &cli.json {
        std::fs::write(path, report.json().pretty())
            .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
    }

    let wall = started.elapsed();
    println!("WALL: {:.2}s", wall.as_secs_f64());
    if wall > Duration::from_secs(120) {
        let mut slow: Vec<_> = report
            .files
            .iter()
            .filter(|file| file.compared() > 0)
            .map(|file| (file.duration, &file.name))
            .collect();
        slow.sort_by_key(|(duration, _)| std::cmp::Reverse(*duration));
        let names: Vec<String> = slow
            .iter()
            .take(8)
            .map(|(duration, name)| format!("{name} {:.1}s", duration.as_secs_f64()))
            .collect();
        println!("SLOWEST FILES: {}", names.join(", "));
    }

    let baseline_path = cli
        .baseline
        .clone()
        .unwrap_or_else(|| workspace.join("crates/baash-spec/baseline.txt"));

    if cli.record {
        let measured = baash_spec::baseline::Baseline::of(&report);
        measured.save(&baseline_path)?;
        println!("RECORDED: {}", baseline_path.display());
    }

    if let Some(min) = cli.min {
        let measured = if cli.strict {
            report.passed_strict()
        } else {
            report.passed()
        };
        if measured < min {
            eprintln!(
                "baash-spec: {} passes is below --min {min}",
                if cli.strict { "strict" } else { "total" }
            );
            return Ok(std::process::ExitCode::from(1));
        }
    }

    if cli.ratchet {
        let baseline = baash_spec::baseline::Baseline::load(&baseline_path)?;
        let measure = if cli.strict {
            baash_spec::baseline::Measure::Strict
        } else {
            baash_spec::baseline::Measure::Total
        };
        let recorded = match measure {
            baash_spec::baseline::Measure::Strict => baseline.strict,
            baash_spec::baseline::Measure::Total => baseline.total,
        };
        let measured = measure.of(&report);
        let verdict = baash_spec::baseline::check(recorded, measured, cli.tolerance);
        let label = if cli.strict { "strict" } else { "total" };
        match verdict {
            baash_spec::baseline::Verdict::Held { delta } => {
                println!("RATCHET: {label} {measured} vs baseline {recorded} ({delta:+})");
            }
            baash_spec::baseline::Verdict::Regressed { delta, missing } => {
                eprintln!(
                    "baash-spec: {label} count {measured} is {missing} below the baseline \
                     {recorded} (tolerance {}) — {delta}",
                    cli.tolerance
                );
                eprintln!("Re-record only if the drop is intended: --record");
                return Ok(std::process::ExitCode::from(1));
            }
        }
    }

    Ok(std::process::ExitCode::SUCCESS)
}

fn select(file: &TestFile, filter: Option<&str>) -> Option<FilePlan> {
    let selected: Vec<usize> = match filter {
        None => (0..file.cases.len()).collect(),
        Some(filter) => {
            if file.name.contains(filter) {
                (0..file.cases.len()).collect()
            } else {
                file.cases
                    .iter()
                    .enumerate()
                    .filter(|(_, case)| case.name.contains(filter))
                    .map(|(index, _)| index)
                    .collect()
            }
        }
    };
    if selected.is_empty() {
        return None;
    }

    // Skip policy (YSH, missing helpers) lives in the runner; selection only
    // decides which cases this invocation looks at.
    Some(FilePlan {
        file: file.clone(),
        selected,
    })
}

fn print_report(report: &Report, list_failures: bool) {
    for file in &report.files {
        let mut line = format!(
            "{:<30} {:>5}/{:<5}",
            file.name,
            file.passed(),
            file.compared()
        );
        if file.skipped() > 0 {
            line.push_str(&format!("  ({} skipped)", file.skipped()));
        }
        println!("{}", line.trim_end());
    }

    println!(
        "STRICT: {}/{} passes count only the primary expectation ({} rely on \
         an `## OK <shell>` alternative)",
        report.passed_strict(),
        report.compared(),
        report.ok_overrides()
    );

    if list_failures {
        for file in &report.files {
            for case in &file.cases {
                if case.kind == OutcomeKind::Pass || case.kind == OutcomeKind::Skip {
                    continue;
                }
                println!();
                println!(
                    "{} {}:{}: {}",
                    case.kind.as_str().to_uppercase(),
                    file.name,
                    case.line,
                    case.name
                );
                if let Some(reason) = &case.reason {
                    println!("  reason: {reason}");
                }
                for detail_line in case.detail.lines() {
                    println!("  {detail_line}");
                }
            }
        }
    }

    println!();
    println!(
        "TOTAL: {}/{} compared, {} skipped, of {} cases",
        report.passed(),
        report.compared(),
        report.skipped(),
        report.all()
    );
    println!(
        "FAILED: {}   TIMEOUTS: {}",
        report.failed(),
        report.timeouts()
    );
    if report.ok_overrides() > 0 {
        println!(
            "OK-OVERRIDE PASSES: {} (accepted a per-shell `## OK ... STDOUT:` alternative)",
            report.ok_overrides()
        );
    }
    if !report.skip_reasons.is_empty() {
        let reasons: Vec<String> = report
            .skip_reasons
            .iter()
            .map(|(reason, count)| format!("{reason}={count}"))
            .collect();
        println!("SKIPS: {}", reasons.join(", "));
    }
    for compare in &report.compare {
        println!(
            "COMPARE {}: {}/{} passed, WE-REGRESS {} (ours fail, reference pass), WE-IMPROVE {}",
            compare.label, compare.passed, compare.compared, compare.we_regress, compare.we_improve
        );
    }
}

fn default_shell(workspace: &Path) -> Result<PathBuf, String> {
    let candidate = workspace.join("target/debug/baash");
    if candidate.is_file() {
        return Ok(candidate);
    }
    eprintln!(
        "baash-spec: {} is missing; running `cargo build -p baash`",
        candidate.display()
    );
    let status = Command::new("cargo")
        .args(["build", "-p", "baash"])
        .current_dir(workspace)
        .status()
        .map_err(|error| format!("could not run cargo: {error}"))?;
    if !status.success() {
        return Err(format!(
            "`cargo build -p baash` failed (status {status}); build the shell first"
        ));
    }
    if !candidate.is_file() {
        return Err(format!(
            "{} still does not exist after building; build the shell first",
            candidate.display()
        ));
    }
    Ok(candidate)
}

fn workspace_root() -> Result<PathBuf, String> {
    if let Some(root) = std::env::var_os("BAASH_SPEC_WORKSPACE_ROOT") {
        return Ok(PathBuf::from(root));
    }
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let root = manifest_dir
        .join("../..")
        .canonicalize()
        .map_err(|error| format!("cannot locate the workspace root: {error}"))?;
    Ok(root)
}

fn resolve_references(tokens: &[String]) -> Result<Vec<Reference>, String> {
    let mut references = Vec::new();
    for token in tokens {
        let token = token.trim();
        if token.is_empty() {
            continue;
        }
        // Absolute but unresolved, for both reasons given at `--shell`: the
        // comparison shell runs in the same per-case directory.
        let path = if token.contains('/') {
            let path = PathBuf::from(token);
            if !path.is_file() {
                return Err(format!("reference shell {token:?} does not exist"));
            }
            std::path::absolute(&path)
                .map_err(|error| format!("cannot resolve {token:?}: {error}"))?
        } else {
            find_on_path(token)
                .ok_or_else(|| format!("reference shell {token:?} was not found on PATH"))?
        };
        let label = path
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| token.to_string());
        if references
            .iter()
            .any(|reference: &Reference| reference.label == label)
        {
            return Err(format!("reference shell {label:?} was listed twice"));
        }
        references.push(Reference { label, path });
    }
    Ok(references)
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            // Absolute (a PATH entry may be relative) but unresolved, so the
            // found name is what runs.
            return std::path::absolute(&candidate).ok();
        }
    }
    None
}

fn parse_args() -> Result<Cli, String> {
    let mut cli = Cli {
        shell: None,
        spec_dir: None,
        filter: None,
        compare: Vec::new(),
        list_failures: false,
        json: None,
        min: None,
        baseline: None,
        record: false,
        ratchet: false,
        strict: false,
        tolerance: 0,
        timeout: Duration::from_secs(10),
        jobs: 4,
        keep_temp: false,
        locale: "C".to_string(),
    };
    let mut args = std::env::args_os().skip(1).peekable();
    while let Some(arg) = args.next() {
        let text = arg.to_string_lossy();
        let mut value = |name: &str| -> Result<OsString, String> {
            args.next()
                .ok_or_else(|| format!("{name} requires a value"))
        };
        match text.as_ref() {
            "-h" | "--help" => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            "--shell" => cli.shell = Some(PathBuf::from(value("--shell")?)),
            "--spec-dir" => cli.spec_dir = Some(PathBuf::from(value("--spec-dir")?)),
            "--filter" => {
                cli.filter = Some(
                    value("--filter")?
                        .to_str()
                        .ok_or("--filter must be valid UTF-8")?
                        .to_string(),
                );
            }
            "--compare" => {
                let raw = value("--compare")?;
                let raw = raw.to_str().ok_or("--compare must be valid UTF-8")?;
                cli.compare
                    .extend(raw.split(',').map(|item| item.trim().to_string()));
            }
            "--list-failures" => cli.list_failures = true,
            "--json" => cli.json = Some(PathBuf::from(value("--json")?)),
            "--baseline" => {
                let raw = value("--baseline")?;
                cli.baseline = Some(PathBuf::from(raw));
            }
            "--record" => cli.record = true,
            "--ratchet" => cli.ratchet = true,
            "--strict" => cli.strict = true,
            "--tolerance" => {
                let raw = value("--tolerance")?;
                cli.tolerance = raw
                    .to_str()
                    .ok_or("--tolerance must be valid UTF-8")?
                    .parse()
                    .map_err(|_| "--tolerance requires a non-negative integer")?;
            }
            "--locale" => {
                let raw = value("--locale")?;
                cli.locale = raw
                    .to_str()
                    .ok_or("--locale must be valid UTF-8")?
                    .to_string();
            }
            "--min" => {
                let raw = value("--min")?;
                cli.min = Some(
                    raw.to_str()
                        .ok_or("--min must be valid UTF-8")?
                        .parse()
                        .map_err(|_| "--min requires a non-negative integer")?,
                );
            }
            "--timeout" => {
                let raw = value("--timeout")?;
                let seconds: f64 = raw
                    .to_str()
                    .ok_or("--timeout must be valid UTF-8")?
                    .parse()
                    .map_err(|_| "--timeout requires a number of seconds")?;
                if !(seconds.is_finite() && seconds > 0.0) {
                    return Err("--timeout must be a positive number".to_string());
                }
                cli.timeout = Duration::from_secs_f64(seconds);
            }
            "--jobs" => {
                let raw = value("--jobs")?;
                let jobs: usize = raw
                    .to_str()
                    .ok_or("--jobs must be valid UTF-8")?
                    .parse()
                    .map_err(|_| "--jobs requires a positive integer")?;
                if jobs == 0 {
                    return Err("--jobs requires a positive integer".to_string());
                }
                cli.jobs = jobs;
            }
            "--keep-temp" => cli.keep_temp = true,
            other => {
                return Err(format!("unrecognized argument {other:?}\n{USAGE}"));
            }
        }
    }
    Ok(cli)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selects_by_file_and_case_name() {
        let file = parse::parse_text(
            "quote.test.sh",
            Path::new("quote.test.sh"),
            "## compare_shells: bash\n\n#### Unquoted words\necho hi\n\n#### Other\necho bye\n",
        )
        .unwrap();
        let plan = select(&file, Some("quote")).unwrap();
        assert_eq!(plan.selected.len(), 2);
        let plan = select(&file, Some("Other")).unwrap();
        assert_eq!(plan.selected, vec![1]);
        assert!(select(&file, Some("absent")).is_none());
    }
}
