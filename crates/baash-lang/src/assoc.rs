//! The associative array: pairs in insertion order, plus a key index.
//!
//! Bash leaves the iteration order of an associative array *unspecified* — it
//! is the hash table's, measured on 5.3.20 as `four two three one` for a table
//! inserted as `one two three four`. An unspecified order is one no script can
//! be tested against, and completion, the linter and the prompt would each see
//! a different one, so `baash` promises insertion order instead
//! (`docs/contracts.md`): a key keeps the position it was first assigned at,
//! `${a[@]}` and `${!a[@]}` walk the keys in that order, and `declare -p` sorts
//! them so its output is stable too.
//!
//! Both halves are `indexmap`'s: the entries vector *is* the order, and the
//! hash index makes `a[k]=v` and `${a[k]}` O(1) instead of a scan of every key
//! (measured: a 1024-key build-up went from 666 µs to 51 µs, and one element
//! write from 650 ns to 49 ns; `docs/performance.md`). The pairs are the truth
//! and the index only decides *where* they are, which is why [`PartialEq`]
//! compares the order and not the index.
//!
//! This lives in `baash-lang` rather than beside its storage in
//! `baash-runtime` because expansion reads arrays through a borrowed
//! `ArrayValue`: a `[(Vec<u8>, Vec<u8>)]` slice cannot carry the index, so
//! `${a[k]}` would keep the linear scan this type exists to remove.
//!
//! The one thing that is deliberately *not* here is the shell's own rule that
//! an empty key is `bad array subscript`; that is `set_assoc`'s job, because it
//! is about the assignment syntax and not about the container.

use ahash::RandomState;
use indexmap::IndexMap;

/// An associative array, in insertion order.
///
/// The key is `Vec<u8>` and not `String`: shell text is bytes (`DESIGN.md`
/// §4.2), and `a[$'\xff']=x` is a legal assignment.
#[derive(Clone, Default)]
pub struct AssocArray(IndexMap<Vec<u8>, Vec<u8>, RandomState>);

impl AssocArray {
    /// An empty array.
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of elements.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the array has no elements.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// One element by key.
    pub fn get(&self, key: &[u8]) -> Option<&[u8]> {
        self.0.get(key).map(Vec::as_slice)
    }

    /// Whether the key is present.
    pub fn contains(&self, key: &[u8]) -> bool {
        self.0.contains_key(key)
    }

    /// Assign a key: a new key goes at the end, an existing one keeps its
    /// position and gets the new value. That is the whole of the order
    /// contract, in one function.
    pub fn insert(&mut self, key: Vec<u8>, value: Vec<u8>) {
        self.0.insert(key, value);
    }

    /// Remove a key, keeping the order of the rest.
    ///
    /// `shift_remove`, not `swap_remove`: swapping the last element into the
    /// hole is cheaper and reorders the array, which is exactly what this type
    /// promises not to do.
    pub fn remove(&mut self, key: &[u8]) -> bool {
        self.0.shift_remove(key).is_some()
    }

    /// Drop every element, keeping the allocation.
    pub fn clear(&mut self) {
        self.0.clear();
    }

    /// The pairs in insertion order.
    pub fn iter(&self) -> impl Iterator<Item = (&Vec<u8>, &Vec<u8>)> {
        self.0.iter()
    }

    /// The keys in insertion order.
    pub fn keys(&self) -> impl Iterator<Item = &Vec<u8>> {
        self.0.keys()
    }

    /// The values in key order.
    pub fn values(&self) -> impl Iterator<Item = &Vec<u8>> {
        self.0.values()
    }

    /// Whether these are the same pairs, in the same order.
    fn same_pairs(&self, other: &[(Vec<u8>, Vec<u8>)]) -> bool {
        self.0.len() == other.len()
            && self
                .0
                .iter()
                .zip(other)
                .all(|((key, value), (other_key, other_value))| {
                    key == other_key && value == other_value
                })
    }

    /// An array from pairs, later duplicates winning at the first position —
    /// the same rule [`insert`](Self::insert) states.
    ///
    /// Tests and the fixture build an expected array this way; the shell
    /// itself assigns one key at a time.
    pub fn from_pairs<I>(pairs: I) -> Self
    where
        I: IntoIterator<Item = (Vec<u8>, Vec<u8>)>,
    {
        let mut array = Self::new();
        for (key, value) in pairs {
            array.insert(key, value);
        }
        array
    }
}

impl std::fmt::Debug for AssocArray {
    /// `[("k", "v"), …]`, not `{"k": "v"}`, because the order is the point.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_list().entries(self.0.iter()).finish()
    }
}

/// Equality is the ordering, and nothing else.
///
/// `IndexMap`'s own `PartialEq` ignores order, which would make a test that
/// pins insertion order pass whatever the array did; comparing the pair
/// sequences keeps the documented promise testable.
impl PartialEq for AssocArray {
    fn eq(&self, other: &Self) -> bool {
        self.0.len() == other.0.len() && self.0.iter().eq(other.0.iter())
    }
}

impl Eq for AssocArray {}

/// Compare against the pairs a test wrote out, in order.
///
/// Without this the runtime's tests would have to build an `AssocArray` to say
/// what they expect, which reads worse than the pairs themselves:
/// `assert_eq!(pairs, &[(b"b".to_vec(), b"20".to_vec()), …])`.
impl PartialEq<[(Vec<u8>, Vec<u8>)]> for AssocArray {
    fn eq(&self, other: &[(Vec<u8>, Vec<u8>)]) -> bool {
        self.same_pairs(other)
    }
}

/// The same, for the array literal a test writes without a `[..]` cast.
impl<const N: usize> PartialEq<[(Vec<u8>, Vec<u8>); N]> for AssocArray {
    fn eq(&self, other: &[(Vec<u8>, Vec<u8>); N]) -> bool {
        self.same_pairs(other)
    }
}

impl FromIterator<(Vec<u8>, Vec<u8>)> for AssocArray {
    fn from_iter<I: IntoIterator<Item = (Vec<u8>, Vec<u8>)>>(pairs: I) -> Self {
        let mut array = Self::new();
        for (key, value) in pairs {
            array.insert(key, value);
        }
        array
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair(key: &[u8], value: &[u8]) -> (Vec<u8>, Vec<u8>) {
        (key.to_vec(), value.to_vec())
    }

    #[test]
    fn a_new_key_joins_the_end_and_an_old_one_keeps_its_place() {
        let mut array = AssocArray::from_pairs([pair(b"b", b"1"), pair(b"a", b"2")]);
        array.insert(b"c".to_vec(), b"3".to_vec());
        array.insert(b"b".to_vec(), b"20".to_vec());
        assert_eq!(
            array,
            [pair(b"b", b"20"), pair(b"a", b"2"), pair(b"c", b"3")],
            "the update kept `b` first, the new key went last"
        );
    }

    #[test]
    fn removal_keeps_the_order_of_the_rest_and_the_index_agrees() {
        let mut array =
            AssocArray::from_pairs([pair(b"a", b"1"), pair(b"b", b"2"), pair(b"c", b"3")]);
        assert!(array.remove(b"b"), "the key was there");
        assert!(!array.remove(b"b"), "and is gone the second time");
        assert_eq!(array, [pair(b"a", b"1"), pair(b"c", b"3")]);
        // The index has to have moved `c` down with its pair; a stale position
        // would read `c`'s old slot, or an out-of-range one.
        assert_eq!(array.get(b"c"), Some(b"3".as_slice()));
        assert_eq!(array.get(b"b"), None);
        array.insert(b"d".to_vec(), b"4".to_vec());
        assert_eq!(
            array,
            [pair(b"a", b"1"), pair(b"c", b"3"), pair(b"d", b"4")],
            "a reinsertion after a removal still appends"
        );
        assert_eq!(array.get(b"d"), Some(b"4".as_slice()));
    }

    #[test]
    fn equality_is_the_order_and_not_the_hash_table() {
        let forward = AssocArray::from_pairs([pair(b"a", b"1"), pair(b"b", b"2")]);
        let backward = AssocArray::from_pairs([pair(b"b", b"2"), pair(b"a", b"1")]);
        // `IndexMap`'s own `PartialEq` ignores order, so this is the assertion
        // that keeps a test which pins insertion order honest.
        assert_ne!(
            forward, backward,
            "the same pairs in another order are not the same array"
        );
        assert_eq!(
            forward,
            AssocArray::from_pairs([pair(b"a", b"1"), pair(b"b", b"2")])
        );
    }

    #[test]
    fn a_repeated_key_in_from_pairs_keeps_its_first_position() {
        let array = AssocArray::from_pairs([pair(b"a", b"1"), pair(b"b", b"2"), pair(b"a", b"3")]);
        assert_eq!(array, [pair(b"a", b"3"), pair(b"b", b"2")]);
    }

    #[test]
    fn an_empty_array_reports_nothing() {
        let mut array = AssocArray::new();
        assert!(array.is_empty());
        assert_eq!(array.len(), 0);
        assert!(!array.contains(b"a"));
        assert_eq!(array.get(b"a"), None);
        assert_eq!(array.keys().count(), 0);
        array.insert(b"a".to_vec(), b"".to_vec());
        assert!(array.contains(b"a"), "an empty value is still an element");
        assert_eq!(array.len(), 1);
        array.clear();
        assert!(array.is_empty());
    }
}
