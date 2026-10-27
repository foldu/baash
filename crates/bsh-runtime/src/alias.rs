//! Alias definitions: a name-to-text table in shell state.
//!
//! An alias is *not* a function. It is a textual substitution performed
//! before the command is parsed, so its value is arbitrary shell text — a
//! whole `for` loop, a pipeline, an unbalanced fragment that only makes sense
//! together with the words that follow. That is why the table lives here (the
//! state) while the substitution itself lives in `bsh-vm` (the parser's
//! front door): this crate cannot expand an alias without re-entering the
//! parser, and a builtin body must not.
//!
//! Listing order is by name, which is what `alias` prints and what bash does;
//! the table keeps itself sorted so the listing costs nothing.

/// The alias table: `(name, value)` pairs, sorted by name.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AliasTable {
    entries: Vec<(Vec<u8>, Vec<u8>)>,
}

impl AliasTable {
    /// An empty table.
    pub fn new() -> Self {
        Self::default()
    }

    /// The value `name` expands to.
    pub fn get(&self, name: &[u8]) -> Option<&[u8]> {
        self.position(name).map(|at| self.entries[at].1.as_slice())
    }

    /// Define or redefine an alias.
    pub fn set(&mut self, name: &[u8], value: Vec<u8>) {
        match self.position(name) {
            Some(at) => self.entries[at].1 = value,
            None => {
                let at = self
                    .entries
                    .binary_search_by(|(existing, _)| existing.as_slice().cmp(name))
                    .unwrap_or_else(|at| at);
                self.entries.insert(at, (name.to_vec(), value));
            }
        }
    }

    /// Remove an alias. Returns whether it existed.
    pub fn remove(&mut self, name: &[u8]) -> bool {
        match self.position(name) {
            Some(at) => {
                self.entries.remove(at);
                true
            }
            None => false,
        }
    }

    /// Remove every alias.
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// The number of definitions.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the table is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Every definition, in name order.
    pub fn iter(&self) -> impl Iterator<Item = (&[u8], &[u8])> {
        self.entries
            .iter()
            .map(|(name, value)| (name.as_slice(), value.as_slice()))
    }

    fn position(&self, name: &[u8]) -> Option<usize> {
        self.entries
            .binary_search_by(|(existing, _)| existing.as_slice().cmp(name))
            .ok()
    }
}

#[cfg(test)]
mod tests {
    use super::AliasTable;

    #[test]
    fn definitions_are_kept_sorted_and_replaced_in_place() {
        let mut table = AliasTable::new();
        table.set(b"ll", b"ls -l".to_vec());
        table.set(b"e", b"echo".to_vec());
        table.set(b"ex", b"exit".to_vec());
        let names: Vec<&[u8]> = table.iter().map(|(name, _)| name).collect();
        assert_eq!(names, [&b"e"[..], b"ex", b"ll"]);

        table.set(b"e", b"echo -e".to_vec());
        assert_eq!(table.get(b"e"), Some(&b"echo -e"[..]));
        assert_eq!(table.len(), 3);
    }

    #[test]
    fn removal_reports_whether_it_happened() {
        let mut table = AliasTable::new();
        table.set(b"a", b"b".to_vec());
        assert!(table.remove(b"a"));
        assert!(!table.remove(b"a"));
        assert!(table.is_empty());
    }
}
