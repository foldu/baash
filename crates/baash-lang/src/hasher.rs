//! The seed every hash table in the shell starts from.
//!
//! `ahash`'s default `RandomState` mixes in a seed read from the operating
//! system once per process. That is the right default for a network service
//! and the wrong one for a shell: the defence is against a flood of keys an
//! attacker picked, and anything that can choose the shell's keys — a variable
//! name, an array subscript — is already running as this shell. What it costs
//! is a `getrandom` on a startup path that is 0.53 ms end to end: 10 µs of it,
//! measured, which is 2% of a start for a defence with nothing to defend
//! (`docs/performance.md`). Bash's own variable hash is fixed too.
//!
//! One place decides, so the variable table and the associative array cannot
//! drift apart.

use ahash::RandomState;

/// A `RandomState` with fixed seeds: no OS read, no per-process cost.
///
/// The constants are a Nothing-up-my-sleeve choice — they are the four
/// 64-bit words of the SHA-512 initial hash block, which have no relation to
/// this program.
pub fn seeded() -> RandomState {
    RandomState::with_seeds(
        0x6a09_e667_f3bc_c908,
        0xbb67_ae85_84ca_a73b,
        0x3c6e_f372_fe94_f82b,
        0xa54f_f53a_5f1d_36f1,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash_of(keys: &[&[u8]]) -> Vec<u64> {
        let state = seeded();
        keys.iter().map(|key| state.hash_one(key)).collect()
    }

    #[test]
    fn the_seed_is_the_same_every_time() {
        // Two calls, same hashes: this is what "no OS read" buys, and what
        // makes a shell's tables reproducible between runs.
        assert_eq!(hash_of(&[b"one", b"two"]), hash_of(&[b"one", b"two"]));
        let hashes = hash_of(&[b"one", b"two", b"three", b"four"]);
        assert_eq!(
            hashes.len(),
            hashes
                .iter()
                .collect::<std::collections::HashSet<_>>()
                .len(),
            "distinct keys should not collide"
        );
    }
}
