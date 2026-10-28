//! Two-tier ranking: typed prefix first, fuzzy second.
//!
//! This ordering is the whole point (`DESIGN.md` §6.4, §6.5):
//!
//! * **tier 0** — the candidate starts with exactly what was typed;
//! * **tier 1** — it starts with it modulo ASCII case;
//! * **tier 2** — `nucleo-matcher` found the characters in order, somewhere.
//!
//! A lower tier always sorts first. `git ch<TAB>` cannot lose to a fuzzy match
//! on some other command, whatever score nucleo gives it. Within a tier,
//! prefixes are ordered by frecency and fuzzy matches by score.
//!
//! `nucleo-matcher` is used directly, not the `nucleo` worker crate: it has no
//! threadpool, which is what `DESIGN.md` §4.3 and §6.5 require. The matcher
//! works on decoded text, so fuzzy scoring sees the candidate's lossy UTF-8
//! projection; the prefix tiers and all insertion work on bytes.

use nucleo_matcher::{Config, Matcher, Utf32Str};

use crate::candidate::{CandKind, Candidate};
use crate::text;

/// Tier value for fuzzy matches that did not match at all; removed before the
/// menu is built.
const NO_MATCH: u8 = u8::MAX;

/// Holds the reusable `nucleo-matcher` instances.
///
/// `Matcher::new` allocates a fairly large score matrix, so one ranker lives for
/// the life of the engine and is reused for every completion. It is deliberately
/// not `Clone`: creating a second one is cheaper than copying the matrix.
pub struct Ranker {
    plain: Matcher,
    path: Matcher,
}

impl Ranker {
    /// A ranker with the configurations `DESIGN.md` §6.5 asks for.
    pub fn new() -> Self {
        let mut plain = Config::DEFAULT;
        plain.prefer_prefix = true;
        let mut path = Config::DEFAULT.match_paths();
        path.prefer_prefix = true;
        Self {
            plain: Matcher::new(plain),
            path: Matcher::new(path),
        }
    }

    /// Score `text` against `needle`, preferring matches near the start.
    ///
    /// Path candidates use [`Config::match_paths`], which adds a bonus after a
    /// `/` and is why `src/ma` ranks `src/main.rs` well.
    ///
    /// The greedy matcher is used deliberately: `nucleo-matcher` 0.3.1's
    /// optimal matcher panics (`should have been caught by prefilter`) on
    /// ordinary ASCII subsequences such as `HOSTNAME` ← `HOM`, and a menu
    /// candidate must never take the shell down. Greedy matching is `O(n)`,
    /// which also bounds the per-keystroke work; the tier rules below — not the
    /// fuzzy score — decide the product behaviour.
    pub fn score(&mut self, text: &str, needle: &str, path: bool) -> Option<u16> {
        let mut haystack_buf = Vec::new();
        let mut needle_buf = Vec::new();
        let haystack = Utf32Str::new(text, &mut haystack_buf);
        let needle = Utf32Str::new(needle, &mut needle_buf);
        if path {
            self.path.fuzzy_match_greedy(haystack, needle)
        } else {
            self.plain.fuzzy_match_greedy(haystack, needle)
        }
    }

    /// Fill in tiers and scores, drop non-matches, and sort.
    pub fn rank(&mut self, candidates: &mut Vec<Candidate>, prefix: &[u8]) {
        let needle = std::str::from_utf8(prefix).ok();
        // `DESIGN.md` §6.5: a path is matched by component prefix, and nucleo
        // ranks only the final component. Feeding it `src/ma` would let the
        // directory part dominate the gap penalties.
        let needle_component = needle.map(|needle| match needle.rsplit_once('/') {
            Some((_, component)) => component,
            None => needle,
        });
        for candidate in candidates.iter_mut() {
            if prefix.is_empty() || text::starts_with(&candidate.insert, prefix) {
                candidate.tier = 0;
                candidate.score = 0;
            } else if text::starts_with_ascii_ci(&candidate.insert, prefix) {
                candidate.tier = 1;
                candidate.score = 0;
            } else {
                let is_path = candidate.kind == CandKind::Path;
                let haystack = if is_path {
                    match candidate.display.rsplit_once('/') {
                        Some((_, component)) => component,
                        None => candidate.display.as_str(),
                    }
                } else {
                    candidate.display.as_str()
                };
                let score =
                    needle_component.and_then(|needle| self.score(haystack, needle, is_path));
                match score {
                    Some(score) => {
                        candidate.tier = 2;
                        candidate.score = score;
                    }
                    None => candidate.tier = NO_MATCH,
                }
            }
        }

        candidates.retain(|candidate| candidate.tier != NO_MATCH);
        candidates.sort_by(|a, b| {
            a.tier
                .cmp(&b.tier)
                .then_with(|| {
                    if a.tier == 2 {
                        b.score.cmp(&a.score)
                    } else {
                        b.frecency.total_cmp(&a.frecency)
                    }
                })
                .then_with(|| b.frecency.total_cmp(&a.frecency))
                .then_with(|| a.display.cmp(&b.display))
                .then_with(|| a.insert.cmp(&b.insert))
        });
    }
}

impl Default for Ranker {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for Ranker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ranker").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn named(insert: &str, frecency: f32) -> Candidate {
        Candidate::new(insert.to_string(), CandKind::Command).with_frecency(frecency)
    }

    #[test]
    fn a_low_frecency_prefix_beats_a_high_scoring_fuzzy_match() {
        let mut ranker = Ranker::new();
        let mut candidates = vec![named("scratch", 1_000.0), named("chunky", 0.0)];
        ranker.rank(&mut candidates, b"ch");
        assert_eq!(candidates[0].insert, b"chunky");
        assert_eq!(candidates[0].tier, 0);
        // `scratch` contains `ch` contiguously, so nucleo does match it — and
        // it still loses.
        assert_eq!(candidates[1].insert, b"scratch");
        assert_eq!(candidates[1].tier, 2);
        assert!(candidates[1].score > 0);
    }

    #[test]
    fn exact_case_prefix_beats_caseless_prefix() {
        let mut ranker = Ranker::new();
        let mut candidates = vec![named("CHUNKY", 500.0), named("chunky", 0.0)];
        ranker.rank(&mut candidates, b"ch");
        assert_eq!(candidates[0].insert, b"chunky");
        assert_eq!(candidates[0].tier, 0);
        assert_eq!(candidates[1].tier, 1);
    }

    #[test]
    fn prefixes_are_ordered_by_frecency() {
        let mut ranker = Ranker::new();
        let mut candidates = vec![named("checkout", 1.0), named("cherry", 9.0)];
        ranker.rank(&mut candidates, b"ch");
        assert_eq!(candidates[0].insert, b"cherry");
        assert_eq!(candidates[1].insert, b"checkout");
    }

    #[test]
    fn non_matches_are_dropped() {
        let mut ranker = Ranker::new();
        let mut candidates = vec![named("zzzz", 100.0)];
        ranker.rank(&mut candidates, b"ch");
        assert!(candidates.is_empty());
    }

    #[test]
    fn empty_prefix_keeps_everything_in_tier_zero() {
        let mut ranker = Ranker::new();
        let mut candidates = vec![named("b", 0.0), named("a", 0.0)];
        ranker.rank(&mut candidates, b"");
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].insert, b"a");
        assert!(candidates.iter().all(|c| c.tier == 0));
    }

    #[test]
    fn nucleo_scores_a_prefix_higher_than_a_stray_match() {
        let mut ranker = Ranker::new();
        let good = ranker.score("main.rs", "ma", true).unwrap();
        let poor = ranker.score("main.rs", "mz", true);
        assert!(poor.is_none() || good > poor.unwrap());
        assert!(ranker.score("main.rs", "rs", true).is_some());
        assert!(ranker.score("abc", "abcd", true).is_none());
    }

    #[test]
    fn path_configuration_prefers_path_components() {
        let mut ranker = Ranker::new();
        // With `match_paths`, the `/` boundary matters, so `sr` matches
        // `src/main.rs` at a component start.
        assert!(ranker.score("src/main.rs", "sr", true).is_some());
        assert!(ranker.score("src/main.rs", "mn", true).is_some());
    }

    #[test]
    fn paths_are_fuzzy_matched_on_their_final_component() {
        let mut ranker = Ranker::new();
        let mut candidates = vec![
            Candidate::new("src/main.rs", CandKind::Path),
            Candidate::new("src/model.rs", CandKind::Path),
        ];
        // `mn` is an in-order subsequence of `main` but not of `model`, so only
        // one candidate survives — scored on the component, not the full path.
        ranker.rank(&mut candidates, b"src/mn");
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].insert, b"src/main.rs");
        assert_eq!(candidates[0].tier, 2);
    }

    #[test]
    fn an_ascii_subsequence_never_panics_the_matcher() {
        // `nucleo-matcher` 0.3.1's optimal matcher panics on this pair; the
        // greedy matcher used here must simply score it.
        let mut ranker = Ranker::new();
        assert!(ranker.score("HOSTNAME", "HOM", false).is_some());
        let mut candidates = vec![named("HOSTNAME", 0.0), named("HOME", 0.0)];
        ranker.rank(&mut candidates, b"HOM");
        assert_eq!(candidates.len(), 2);
        assert_eq!(
            candidates[0].insert, b"HOME",
            "the prefix match comes first"
        );
    }

    #[test]
    fn invalid_utf8_prefix_cannot_fuzzy_match() {
        let mut ranker = Ranker::new();
        let mut candidates = vec![named("checkout", 0.0), named("zzz", 0.0)];
        ranker.rank(&mut candidates, b"\xff");
        // Nothing prefix-matches, and the needle is not UTF-8, so everything is
        // dropped rather than matched against a lossy needle.
        assert!(candidates.is_empty());
    }
}
