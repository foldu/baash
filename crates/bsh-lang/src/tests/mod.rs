//! Test suite: table-driven expansion and quoting tests.
//!
//! Every case is written as shell source and parsed by the fixture's small
//! reference parser, so the test exercises the same [`Word`] shape the front
//! end will produce. The expected results were checked against bash 5.3.20
//! (the version DESIGN.md §3.3 verified its Tier C rows against), except where
//! a comment says the behaviour is a documented deviation.

use crate::fixture::{TestShell, parse_word, parse_words};
use crate::{expand_word, expand_words};

mod at_star;
mod params;
mod pipeline;
mod quoting;
mod splitting;

/// Expand one word of source and return argv.
fn expand_one(src: &str, shell: &mut TestShell) -> Vec<Vec<u8>> {
    expand_one_bytes(src.as_bytes(), shell)
}

/// Expand one word of source, byte for byte.
fn expand_one_bytes(src: &[u8], shell: &mut TestShell) -> Vec<Vec<u8>> {
    let word = parse_word(src);
    expand_word(&word, shell).expect("word should expand")
}

/// Expand a whole command line and return argv.
fn expand_line(src: &str, shell: &mut TestShell) -> Vec<Vec<u8>> {
    let words = parse_words(src.as_bytes());
    expand_words(&words, shell).expect("line should expand")
}

/// Try to expand a whole line, keeping the error.
fn try_expand_line(src: &str, shell: &mut TestShell) -> Result<Vec<Vec<u8>>, crate::Error> {
    let words = parse_words(src.as_bytes());
    expand_words(&words, shell)
}

/// A byte-string argv for comparisons.
fn argv(parts: &[&str]) -> Vec<Vec<u8>> {
    parts.iter().map(|part| part.as_bytes().to_vec()).collect()
}

/// A shell value list for comparisons.
fn values(parts: &[&[u8]]) -> Vec<Vec<u8>> {
    parts.iter().map(|part| part.to_vec()).collect()
}

/// Format argv for assertion messages.
fn show(parts: &[Vec<u8>]) -> String {
    let mut out = String::from("argc=");
    out.push_str(&parts.len().to_string());
    for part in parts {
        out.push_str(&format!(" <{}>", String::from_utf8_lossy(part)));
    }
    out
}

/// Assert argv equality with a readable message.
fn assert_argv(got: &[Vec<u8>], want: &[&str], context: &str) {
    assert_eq!(got, argv(want).as_slice(), "{context}: got {}", show(got));
}

#[allow(dead_code)]
fn assert_values(got: &[Vec<u8>], want: &[&[u8]], context: &str) {
    assert_eq!(got, values(want).as_slice(), "{context}: got {}", show(got));
}
