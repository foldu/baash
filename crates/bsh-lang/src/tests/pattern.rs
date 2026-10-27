//! Rules that hold for every pattern operator.

use crate::fixture::TestShell;

use super::{expand_one, try_expand_line};

#[test]
fn pattern_operators_read_the_value_under_nounset() {
    // Every one of these reads the parameter, so `set -u` must fail rather
    // than expand an unset name to nothing.
    for source in ["${u#x}", "${u##x}", "${u%x}", "${u/ x/y}", "${u^^}"] {
        let mut shell = TestShell::new().nounset(true);
        let err = try_expand_line(source, &mut shell)
            .err()
            .unwrap_or_else(|| panic!("{source} should fail under nounset"));
        assert_eq!(err.code(), crate::codes::PARAMETER_UNSET, "{source}");
    }
    // `${a[@]#x}` enumerates instead of reading, so an unset array is fine.
    let mut shell = TestShell::new().nounset(true);
    let got = expand_one("\"${a[@]#x}\"", &mut shell);
    assert!(got.is_empty(), "unset array under nounset: {got:?}");
}
