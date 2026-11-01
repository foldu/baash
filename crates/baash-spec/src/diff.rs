//! Minimal unified-diff rendering for stdout/stderr mismatches.
//!
//! This is deliberately self-contained: the harness has no dependencies and
//! the diff is only for human-readable failure reports.  Very large line
//! counts fall back to a coarse "all removed / all added" middle section.

/// Render a unified diff of two byte strings, truncated to `max_lines` lines.
pub fn unified_diff(expected: &[u8], actual: &[u8], max_lines: usize) -> String {
    let a = split_lines(expected);
    let b = split_lines(actual);

    let ops = edit_script(&a, &b);
    let hunks = hunks(&ops);

    let mut out = String::new();
    out.push_str("--- expected\n+++ actual\n");

    let mut lines = 2usize;
    'hunks: for (start, end) in hunks {
        if lines >= max_lines {
            out.push_str("... (diff truncated)\n");
            break;
        }
        let (old_start, old_count, new_start, new_count) = hunk_ranges(&ops, start, end);
        out.push_str(&format!(
            "@@ -{} +{} @@\n",
            range(old_start, old_count),
            range(new_start, new_count)
        ));
        lines += 1;
        for op in &ops[start..end] {
            if lines >= max_lines || out.len() > 16 * 1024 {
                out.push_str("... (diff truncated)\n");
                break 'hunks;
            }
            let (prefix, line) = match *op {
                Op::Equal(i, _) => (' ', a[i]),
                Op::Delete(i) => ('-', a[i]),
                Op::Insert(_, j) => ('+', b[j]),
            };
            out.push(prefix);
            out.push_str(&display_line(line));
            out.push('\n');
            if !line.ends_with(b"\n") {
                out.push_str("\\ No newline at end of file\n");
                lines += 1;
            }
            lines += 1;
        }
    }

    out
}

/// Total byte count of `bytes`, useful in failure messages.
pub fn one_line_preview(bytes: &[u8], max: usize) -> String {
    let mut s = String::new();
    for line in bytes.split(|&b| b == b'\n').take(1) {
        s.push_str(&display_line(line));
    }
    if s.len() > max {
        s.truncate(max);
        s.push('…');
    }
    s
}

fn range(start: usize, count: usize) -> String {
    if count == 0 {
        format!("{start},0")
    } else {
        format!("{},{}", start + 1, count)
    }
}

fn split_lines(bytes: &[u8]) -> Vec<&[u8]> {
    let mut lines = Vec::new();
    let mut start = 0;
    for (i, &byte) in bytes.iter().enumerate() {
        if byte == b'\n' {
            lines.push(&bytes[start..=i]);
            start = i + 1;
        }
    }
    if start < bytes.len() {
        lines.push(&bytes[start..]);
    }
    lines
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Equal(usize, usize),
    Delete(usize),
    Insert(usize, usize),
}

impl Op {
    fn is_change(self) -> bool {
        !matches!(self, Self::Equal(..))
    }
}

fn edit_script(a: &[&[u8]], b: &[&[u8]]) -> Vec<Op> {
    let m = a.len();
    let n = b.len();

    // Bound the quadratic table.  The fallback is still a valid diff, just a
    // coarse one.
    if m.saturating_mul(n) > 4_000_000 {
        let mut ops = Vec::new();
        let mut prefix = 0;
        while prefix < m && prefix < n && a[prefix] == b[prefix] {
            ops.push(Op::Equal(prefix, prefix));
            prefix += 1;
        }
        let mut suffix = 0;
        while suffix < m - prefix && suffix < n - prefix && a[m - 1 - suffix] == b[n - 1 - suffix] {
            suffix += 1;
        }
        for i in prefix..m - suffix {
            ops.push(Op::Delete(i));
        }
        for j in prefix..n - suffix {
            ops.push(Op::Insert(m, j));
        }
        for k in 0..suffix {
            ops.push(Op::Equal(m - suffix + k, n - suffix + k));
        }
        return ops;
    }

    // dp[i][j] = LCS length of a[i..] and b[j..].
    let width = n + 1;
    let mut dp = vec![0u32; (m + 1) * width];
    for i in (0..m).rev() {
        for j in (0..n).rev() {
            dp[i * width + j] = if a[i] == b[j] {
                dp[(i + 1) * width + j + 1] + 1
            } else {
                dp[(i + 1) * width + j].max(dp[i * width + j + 1])
            };
        }
    }

    let mut ops = Vec::with_capacity(m + n);
    let (mut i, mut j) = (0, 0);
    while i < m && j < n {
        if a[i] == b[j] {
            ops.push(Op::Equal(i, j));
            i += 1;
            j += 1;
        } else if dp[(i + 1) * width + j] >= dp[i * width + j + 1] {
            ops.push(Op::Delete(i));
            i += 1;
        } else {
            ops.push(Op::Insert(i, j));
            j += 1;
        }
    }
    while i < m {
        ops.push(Op::Delete(i));
        i += 1;
    }
    while j < n {
        ops.push(Op::Insert(i, j));
        j += 1;
    }
    ops
}

const CONTEXT: usize = 3;

fn hunks(ops: &[Op]) -> Vec<(usize, usize)> {
    let mut include = vec![false; ops.len()];
    let mut saw_change = false;
    for (index, op) in ops.iter().enumerate() {
        if op.is_change() {
            saw_change = true;
            let start = index.saturating_sub(CONTEXT);
            let end = (index + CONTEXT + 1).min(ops.len());
            for slot in &mut include[start..end] {
                *slot = true;
            }
        }
    }
    if !saw_change {
        return Vec::new();
    }

    let mut hunks = Vec::new();
    let mut index = 0;
    while index < ops.len() {
        if !include[index] {
            index += 1;
            continue;
        }
        let start = index;
        while index < ops.len() && include[index] {
            index += 1;
        }
        hunks.push((start, index));
    }
    hunks
}

fn hunk_ranges(ops: &[Op], start: usize, end: usize) -> (usize, usize, usize, usize) {
    let old_before = ops[..start]
        .iter()
        .filter(|op| matches!(op, Op::Equal(..) | Op::Delete(..)))
        .count();
    let new_before = ops[..start]
        .iter()
        .filter(|op| matches!(op, Op::Equal(..) | Op::Insert(..)))
        .count();
    let old_count = ops[start..end]
        .iter()
        .filter(|op| matches!(op, Op::Equal(..) | Op::Delete(..)))
        .count();
    let new_count = ops[start..end]
        .iter()
        .filter(|op| matches!(op, Op::Equal(..) | Op::Insert(..)))
        .count();
    (old_before, old_count, new_before, new_count)
}

fn display_line(line: &[u8]) -> String {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    let mut out = String::new();
    for c in String::from_utf8_lossy(line).chars() {
        match c {
            '\t' => out.push('\t'),
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x20 || c == '\u{7f}' => {
                out.push_str(&format!("\\x{:02x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_is_empty() {
        let diff = unified_diff(b"a\nb\n", b"a\nb\n", 50);
        assert_eq!(diff, "--- expected\n+++ actual\n");
    }

    #[test]
    fn shows_changed_line() {
        let diff = unified_diff(b"a\nb\nc\n", b"a\nX\nc\n", 50);
        assert!(diff.contains("-b\n"), "{diff}");
        assert!(diff.contains("+X\n"), "{diff}");
    }

    #[test]
    fn distinguishes_missing_trailing_newline() {
        let diff = unified_diff(b"a\n", b"a", 50);
        assert!(diff.contains("No newline"), "{diff}");
    }

    #[test]
    fn truncates() {
        let expected = "a\n".repeat(100);
        let actual = "b\n".repeat(100);
        let diff = unified_diff(expected.as_bytes(), actual.as_bytes(), 10);
        assert!(diff.contains("diff truncated"), "{diff}");
        assert!(diff.lines().count() <= 12, "{diff}");
    }
}
