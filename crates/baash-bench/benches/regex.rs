//! Benchmark the two halves of `[[ text =~ re ]]`.
//!
//! The engine level times `baash_sys::Regex` itself: compile a pattern, and
//! match with one already compiled. Compiling is what the interpreter's
//! `regex::Cache` takes out of a loop, so "compile" is what a loop paid before
//! the memo and "match" is what it pays now — the memo's effect, measured
//! without needing a cache-removed build.
//!
//! The shell level (`--shell PATH`) is the number `docs/performance.md`
//! records: one `-c` script per pattern doing 20 000 `=~`, run against bash and
//! against this shell with each engine, minimum over N interleaved process
//! runs, minus the identical loop whose body is `[[ -n $s ]]`. Interleaved
//! because the three shells then see the same machine over the minutes the run
//! takes; minimum because the distribution is one-sided — the machine can slow
//! a run down, never speed one up — so the minimum is the closest estimate of
//! the shell's own cost.
//!
//! `harness = false` because libtest's `#[bench]` needs nightly and criterion
//! is a large dependency tree for two tables. `Instant::now` around a loop is
//! all these numbers need, and the output is markdown on purpose: it is meant
//! to be pasted into `docs/performance.md`.
//!
//! `just bench` runs both halves. By hand:
//!
//! ```text
//! cargo bench -p baash-bench                       # engine level only
//! cargo bench -p baash-bench -- --shell target/release/baash
//! ```

#![forbid(unsafe_code)]

use std::fmt::Write as _;
use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use baash_sys::{Engine, Regex};

/// `=~` evaluations per timed script run. The recorded table's number; keep it
/// in step with `docs/performance.md` or the two are not comparable.
const EVALS: usize = 20_000;

/// Shell-level repetitions per (script, shell).
const REPS: usize = 11;

/// Engine-level repetitions per measurement.
const ENGINE_REPS: usize = 7;

/// One `=~` shape, with the subject the recorded table uses.
///
/// The subject is the one a completion would test, and it *matches* for every
/// shape but `no-match`, so the match (and, for `capture`, filling the capture
/// slots) is part of the timed work.
struct Shape {
    /// Row label, markdown-escaped for a table cell.
    label: &'static str,
    pattern: &'static str,
    subject: &'static str,
}

const SHAPES: &[Shape] = &[
    Shape {
        label: "`create` (literal)",
        pattern: "create",
        subject: "  -c, --create               create a new archive",
    },
    Shape {
        // The pattern that started the engine question: bash's own
        // tar-completion ERE, 84 bytes, wide bracket classes.
        label: "tar's `_longopt` ERE (84 B)",
        pattern: "^[[:blank:]]{1,10}(((,[[:blank:]])?(--?([A-Za-z0-9?=-]+))(,[[:space:]])?)+).*$",
        subject: "  -c, --create               create a new archive",
    },
    Shape {
        label: "`^(foo|foobar|baz)([0-9]+)?$`",
        pattern: "^(foo|foobar|baz)([0-9]+)?$",
        subject: "foobar42",
    },
    Shape {
        label: "`^--(no-)?file`",
        pattern: "^--(no-)?file",
        subject: "--no-files",
    },
    Shape {
        label: "no-match `^[[:blank:]]*--([[:alnum:]-]+)$`",
        pattern: "^[[:blank:]]*--([[:alnum:]-]+)$",
        subject: "  -c, --create               create a new archive",
    },
    Shape {
        label: "capture `^--([[:alnum:]-]+)=(.*)$`",
        pattern: "^--([[:alnum:]-]+)=(.*)$",
        subject: "--file=some/path/name",
    },
];

fn main() {
    let mut shell: Option<PathBuf> = None;
    let mut bash: Option<PathBuf> = None;
    let mut reps = REPS;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--shell" => shell = args.next().map(PathBuf::from),
            "--bash" => {
                bash = args
                    .next()
                    .filter(|path| !path.is_empty())
                    .map(PathBuf::from);
            }
            "--reps" => {
                reps = args
                    .next()
                    .and_then(|n| n.parse().ok())
                    .unwrap_or_else(|| fail("--reps needs a number"));
            }
            // What `cargo bench` and libtest would hand a harness; there is no
            // libtest here, so they mean nothing.
            "--bench" | "--test" | "--nocapture" => {}
            other => fail(&format!(
                "unknown argument `{other}`\n\n  \
                 cargo bench -p baash-bench [-- --shell <baash>] [--bash <bash>] [--reps N]"
            )),
        }
    }

    let cores = std::thread::available_parallelism().map_or(0, |n| n.get());
    println!(
        "baash-bench: {EVALS} evaluations per run, min of {ENGINE_REPS} / {reps} runs, {cores} cores"
    );
    if let Some(path) = &shell {
        println!("  shell: {}", path.display());
    }
    if let Some(path) = &bash {
        println!("  bash:  {} ({})", path.display(), version(path));
    }
    println!();

    engine_level();

    match shell {
        Some(shell) => shell_level(&shell, bash.as_deref(), reps),
        None => eprintln!(
            "shell level skipped: pass `--shell <path>` (and `--bash <path>`) to \
             measure the loop itself; `just bench` does both"
        ),
    }
}

/// `baash_sys::Regex` per pattern: compile, and match with a compiled one.
///
/// "compile" is what a loop paid before the interpreter cached the pattern,
/// "match" is what it pays now, so the two columns together are the memo's
/// effect: `match` against `compile + match`. A third column measuring both in
/// one loop was tried and dropped — it comes out at 2-3x the sum of the parts,
/// because allocator behaviour inside such a loop differs, and a column whose
/// number cannot be explained is worse than no column.
fn engine_level() {
    println!("## engine level: `baash_sys::Regex`, ns per call");
    println!();
    for engine in [Engine::System, Engine::Minrx] {
        if !engine.is_available() {
            println!("`{}` is not available on this platform.", engine.name());
            println!();
            continue;
        }
        println!("**{}**", engine.name());
        println!();
        println!("| pattern | compile | match |");
        println!("|---|---:|---:|");
        for shape in SHAPES {
            let pattern = shape.pattern.as_bytes();
            let subject = shape.subject.as_bytes();
            let compiled = Regex::with_engine(engine, pattern, false)
                .unwrap_or_else(|error| fail(&format!("{}: {error}", shape.label)));
            black_box(compiled.captures(subject));

            let compile = best_of(ENGINE_REPS, || {
                black_box(Regex::with_engine(engine, pattern, false).ok());
            });
            let matched = best_of(ENGINE_REPS, || {
                black_box(compiled.captures(subject));
            });
            println!(
                "| {} | {} | {} |",
                shape.label,
                compile.as_nanos(),
                matched.as_nanos()
            );
        }
        println!();
    }
}

/// The whole shell: `=~` in a loop, against bash and against each engine.
fn shell_level(shell: &Path, bash: Option<&Path>, reps: usize) {
    let mut columns: Vec<Column> = Vec::new();
    if let Some(bash) = bash {
        columns.push(Column {
            label: "bash",
            program: bash.to_owned(),
            args: Vec::new(),
        });
    }
    if Engine::System.is_available() {
        columns.push(Column {
            label: "system engine",
            program: shell.to_owned(),
            args: Vec::new(),
        });
    }
    if Engine::Minrx.is_available() {
        columns.push(Column {
            label: "MinRX",
            program: shell.to_owned(),
            args: vec!["--regex-engine".to_owned(), "minrx".to_owned()],
        });
    }
    if columns.is_empty() {
        fail("no engine is available to measure");
    }

    let tar = SHAPES[1].subject;

    // Every script is measured inside the same interleaved loop. Measuring the
    // baseline separately would let drift between the two passes land in the
    // subtraction, which is the one number in the table that is a *difference*
    // and therefore the one that cannot absorb it.
    let mut scripts: Vec<(&str, String)> = vec![(
        "base",
        // What the loop, the arithmetic and the process start cost: subtracted
        // so a row is the cost of `=~` itself. Same subject, because the shape
        // of the string can matter (`tar` skips leading blanks it never sees).
        script(tar, "[[ -n $s ]]", EVALS),
    )];
    for shape in SHAPES {
        let body = format!("[[ $s =~ {} ]]", shape.pattern);
        scripts.push((shape.label, script(shape.subject, &body, EVALS)));
    }
    // A loop that rotates two patterns: with one cache slot it misses every
    // iteration; with sixteen both stay compiled.
    scripts.push((
        "two alternating patterns",
        script(
            tar,
            &format!(
                "[[ $s =~ {} ]]\n\x20   [[ $s =~ {} ]]",
                SHAPES[3].pattern, SHAPES[1].pattern
            ),
            EVALS,
        ),
    ));
    scripts.push((
        "rotating two patterns, base",
        script(tar, "[[ -n $s ]]\n\x20   [[ -n $s ]]", EVALS),
    ));

    let mut best: Vec<Vec<Duration>> = vec![vec![Duration::MAX; columns.len()]; scripts.len()];
    // Interleaved on both axes: the shells run back to back inside each
    // repetition, so a machine that drifts over the minutes the run takes
    // drifts for all of them.
    for _ in 0..reps {
        for (index, (_, script)) in scripts.iter().enumerate() {
            for (column, slot) in columns.iter().zip(best[index].iter_mut()) {
                *slot = (*slot).min(run(&column.program, &column.args, script));
            }
        }
    }

    let base = &best[0];
    let per_column = |row: &[Duration], evals: usize| -> Vec<f64> {
        row.iter()
            .zip(base)
            .map(|(time, base)| per_eval(*time, *base, evals))
            .collect()
    };

    println!("## shell level: ns per `=~`, min of {reps} interleaved runs");
    println!();
    let mut header = String::from("| pattern |");
    let mut rule = String::from("|---|");
    for column in &columns {
        let _ = write!(header, " {} |", column.label);
        rule.push_str("---:|");
    }
    println!("{header}");
    println!("{rule}");
    // Rows 1..=SHAPES.len() are the shapes; the last two are the rotation.
    for (index, (label, _)) in scripts.iter().enumerate().skip(1).take(SHAPES.len()) {
        let mut line = format!("| {label} |");
        for value in per_column(&best[index], EVALS) {
            let _ = write!(line, " {value:.0} |");
        }
        println!("{line}");
    }
    println!();
    println!("rotating two patterns (`^--(no-)?file`, then the tar ERE):");
    println!();
    println!("{header}");
    println!("{rule}");
    let rotate = scripts.len() - 2;
    let mut line = format!("| {} |", scripts[rotate].0);
    for value in per_column(&best[rotate], 2 * EVALS) {
        let _ = write!(line, " {value:.0} |");
    }
    println!("{line}");
    println!();
}

/// One measured program: a shell plus the arguments that pick its engine.
struct Column {
    label: &'static str,
    program: PathBuf,
    args: Vec<String>,
}

/// The loop, with `body` as the timed statement and `subject` in `s`.
fn script(subject: &str, body: &str, evals: usize) -> String {
    format!(
        "i=0\n\
         s='{subject}'\n\
         while [ \"$i\" -lt {evals} ]; do\n\
         \x20   {body}\n\
         \x20   i=$((i + 1))\n\
         done\n"
    )
}

fn run(program: &Path, args: &[String], script: &str) -> Duration {
    let start = Instant::now();
    let output = Command::new(program)
        .args(args)
        .arg("-c")
        .arg(script)
        .output()
        .unwrap_or_else(|error| fail(&format!("cannot run {}: {error}", program.display())));
    let elapsed = start.elapsed();
    if !output.status.success() {
        fail(&format!(
            "{} exited with {}:\n{}",
            program.display(),
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    if !output.stdout.is_empty() {
        fail(&format!(
            "{} printed to stdout, which would inflate the timing: {}",
            program.display(),
            String::from_utf8_lossy(&output.stdout)
        ));
    }
    elapsed
}

fn best_of<F: FnMut()>(reps: usize, mut f: F) -> Duration {
    let mut best = Duration::MAX;
    for _ in 0..reps {
        let start = Instant::now();
        f();
        best = best.min(start.elapsed());
    }
    best
}

/// `=~` cost for one run: the timed loop minus the baseline, per evaluation.
fn per_eval(timed: Duration, base: Duration, evals: usize) -> f64 {
    (timed.as_nanos() as f64 - base.as_nanos() as f64) / evals as f64
}

fn version(program: &Path) -> String {
    Command::new(program)
        .arg("--version")
        .output()
        .ok()
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .and_then(|text| text.lines().next().map(str::to_owned))
        .unwrap_or_else(|| "version unknown".to_owned())
}

fn fail(message: &str) -> ! {
    eprintln!("baash-bench: {message}");
    std::process::exit(2)
}
