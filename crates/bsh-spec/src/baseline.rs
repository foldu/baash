//! The conformance ratchet.
//!
//! A number in a comment is a wish; a number in a file that a build compares
//! against is a ratchet. This module owns the file format, the comparison and
//! the decision, and nothing else — so the rule can be unit-tested without
//! running a single spec case.
//!
//! Two counts are recorded, because they answer different questions:
//!
//! * `total` accepts every `## OK <shell> STDOUT:` alternative the suite
//!   offers. It is the fair progress metric for a new shell: a block written
//!   before ßhell existed cannot name it, so requiring only the primary
//!   expectation would punish us for matching a behaviour the suite itself
//!   declares correct.
//! * `strict` counts only the primary expectation. It is the number to quote
//!   when claiming conformance, because it is what an upstream runner would
//!   award a shell whose name appears in no block.
//!
//! Both are recorded and both are checked, so the difference cannot quietly
//! grow: a change that trades strict passes for alternatives shows up as a
//! strict regression even when the total rises.

use std::fmt;
use std::path::Path;

use crate::runner::Report;

/// A recorded conformance baseline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Baseline {
    /// Passes counting every accepted alternative.
    pub total: usize,
    /// Passes counting only the primary expectation.
    pub strict: usize,
}

impl Baseline {
    /// Parse the `key = value` format written by [`Baseline::render`].
    ///
    /// Unknown keys are an error rather than ignored: silently ignoring a key
    /// is how a baseline file ends up recording something nobody checks.
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut total = None;
        let mut strict = None;
        for (index, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                return Err(format!("line {}: expected `key = value`", index + 1));
            };
            let value: usize = value
                .trim()
                .parse()
                .map_err(|_| format!("line {}: `{}` is not a count", index + 1, value.trim()))?;
            match key.trim() {
                "total" => total = Some(value),
                "strict" => strict = Some(value),
                other => return Err(format!("line {}: unknown key `{other}`", index + 1)),
            }
        }
        Ok(Self {
            total: total.ok_or("missing `total`")?,
            strict: strict.ok_or("missing `strict`")?,
        })
    }

    /// Render the file, with the provenance a reader needs to trust it.
    pub fn render(self) -> String {
        format!(
            "# Conformance baseline for `bsh-spec --ratchet`.\n\
             #\n\
             # Re-record with `bsh-spec --record` after a change that improves the\n\
             # numbers, and commit the result. `total` accepts `## OK <shell>`\n\
             # alternatives; `strict` counts only the primary expectation.\n\
             total = {}\n\
             strict = {}\n",
            self.total, self.strict
        )
    }

    /// Read a baseline from disk.
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|error| format!("cannot read baseline {}: {error}", path.display()))?;
        Self::parse(&text).map_err(|error| format!("baseline {}: {error}", path.display()))
    }

    /// Write a baseline to disk.
    pub fn save(self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
        }
        std::fs::write(path, self.render())
            .map_err(|error| format!("cannot write {}: {error}", path.display()))
    }

    /// The baseline measured from a finished run.
    pub fn of(report: &Report) -> Self {
        Self {
            total: report.passed(),
            strict: report.passed_strict(),
        }
    }
}

/// Which count the ratchet enforces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Measure {
    /// Accepts `## OK <shell>` alternatives (the progress metric).
    Total,
    /// Primary expectation only (the conformance claim).
    Strict,
}

impl Measure {
    /// The value this measure reads from a report.
    pub fn of(self, report: &Report) -> usize {
        match self {
            Measure::Total => report.passed(),
            Measure::Strict => report.passed_strict(),
        }
    }
}

/// The ratchet's decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// At or above the baseline, within tolerance.
    Held {
        /// Difference from the baseline (negative is a tolerated dip).
        delta: isize,
    },
    /// Below the baseline by more than the tolerance.
    Regressed {
        /// Difference from the baseline (negative).
        delta: isize,
        /// How many cases short of the tolerance-adjusted floor.
        missing: usize,
    },
}

impl Verdict {
    /// Whether the run may proceed.
    pub fn is_ok(self) -> bool {
        matches!(self, Verdict::Held { .. })
    }
}

impl fmt::Display for Verdict {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Verdict::Held { delta } => write!(out, "held ({delta:+})"),
            Verdict::Regressed { delta, missing } => {
                write!(out, "REGRESSED ({delta:+}, {missing} short)")
            }
        }
    }
}

/// Compare a measurement against the baseline.
///
/// `tolerance` absorbs measured environment jitter — a case that is known to
/// be load-sensitive, say — and nothing more. It defaults to zero at the call
/// site so the tolerance has to be argued for rather than inherited.
pub fn check(baseline: usize, measured: usize, tolerance: usize) -> Verdict {
    let floor = baseline.saturating_sub(tolerance);
    if measured >= floor {
        Verdict::Held {
            delta: measured as isize - baseline as isize,
        }
    } else {
        Verdict::Regressed {
            delta: measured as isize - baseline as isize,
            missing: floor - measured,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_the_file_format() {
        let baseline = Baseline {
            total: 1677,
            strict: 1623,
        };
        let text = baseline.render();
        assert_eq!(Baseline::parse(&text).expect("parse"), baseline);
    }

    #[test]
    fn ignores_comments_and_blank_lines() {
        let parsed = Baseline::parse("# c\n\ntotal = 5\nstrict = 4\n").expect("parse");
        assert_eq!(
            parsed,
            Baseline {
                total: 5,
                strict: 4
            }
        );
    }

    #[test]
    fn rejects_missing_unknown_and_malformed_keys() {
        assert!(Baseline::parse("total = 5\n").is_err(), "missing strict");
        assert!(Baseline::parse("total = 5\nstrict = 4\nnope = 1\n").is_err());
        assert!(Baseline::parse("total = five\nstrict = 4\n").is_err());
        assert!(Baseline::parse("total 5\n").is_err());
    }

    #[test]
    fn a_regression_beyond_tolerance_fails_and_a_dip_within_it_does_not() {
        assert_eq!(check(100, 100, 0), Verdict::Held { delta: 0 });
        assert_eq!(check(100, 120, 0), Verdict::Held { delta: 20 });
        // A tolerated dip still reports its real delta.
        assert_eq!(check(100, 98, 5), Verdict::Held { delta: -2 });
        assert_eq!(
            check(100, 90, 5),
            Verdict::Regressed {
                delta: -10,
                missing: 5
            }
        );
        assert!(!check(100, 90, 5).is_ok());
    }

    #[test]
    fn tolerance_cannot_go_below_zero() {
        // A tolerance larger than the baseline floors at zero rather than
        // underflowing.
        assert_eq!(check(3, 0, 10), Verdict::Held { delta: -3 });
    }
}
