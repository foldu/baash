//! Byte helpers shared by providers and ranking.

/// Whether `haystack` starts with `needle`.
pub(crate) fn starts_with(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.len() >= needle.len() && &haystack[..needle.len()] == needle
}

/// Whether `haystack` starts with `needle`, folding ASCII case only.
///
/// Shell text is bytes. Folding more than ASCII would mean decoding UTF-8 and
/// changing byte offsets, so a non-ASCII prefix only matches byte-for-byte.
pub(crate) fn starts_with_ascii_ci(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.len() >= needle.len()
        && haystack[..needle.len()]
            .iter()
            .zip(needle)
            .all(|(a, b)| a.eq_ignore_ascii_case(b))
}

/// Whether a typed prefix names a path rather than a command.
///
/// `./x`, `/usr/bi` and `~/Doc` are paths even in command position; a bare
/// `git` is not.
pub(crate) fn looks_like_path(prefix: &[u8]) -> bool {
    prefix.first() == Some(&b'/')
        || prefix.first() == Some(&b'~')
        || prefix.first() == Some(&b'.')
        || prefix.contains(&b'/')
}
