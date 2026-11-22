//! A small cache bounded by entry count and by bytes (TRANSCRIPT-16.4, ARCH-12): the least
//! recently used entry goes first. The renderers hold a few hundred entries, so eviction scans
//! rather than keeping a linked list.

use std::collections::HashMap;
use std::hash::Hash;

/// A least-recently-used cache with a cap on entries and on the summed cost of its values.
#[derive(Debug)]
pub(crate) struct Lru<K, V> {
    entries: HashMap<K, Entry<V>>,
    max_entries: usize,
    max_cost: usize,
    cost: usize,
    clock: u64,
}

#[derive(Debug)]
struct Entry<V> {
    value: V,
    cost: usize,
    used: u64,
}

impl<K: Hash + Eq + Clone, V> Lru<K, V> {
    /// A cache holding at most `max_entries` values costing at most `max_cost` together. A single
    /// value dearer than `max_cost` is not kept.
    pub(crate) fn new(max_entries: usize, max_cost: usize) -> Self {
        Self {
            entries: HashMap::new(),
            max_entries,
            max_cost,
            cost: 0,
            clock: 0,
        }
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    /// The summed cost of what is held.
    #[cfg(test)]
    pub(crate) fn cost(&self) -> usize {
        self.cost
    }

    /// Takes the value for `key` out.
    pub(crate) fn remove(&mut self, key: &K) -> Option<V> {
        self.entries.remove(key).map(|entry| {
            self.cost -= entry.cost;
            entry.value
        })
    }

    /// The value for `key`, marking it used.
    pub(crate) fn get(&mut self, key: &K) -> Option<&V> {
        self.clock += 1;
        let clock = self.clock;
        self.entries.get_mut(key).map(|entry| {
            entry.used = clock;
            &entry.value
        })
    }

    /// The value for `key` without marking it used.
    #[cfg(test)]
    pub(crate) fn peek(&self, key: &K) -> Option<&V> {
        self.entries.get(key).map(|entry| &entry.value)
    }

    /// Keeps `value` for `key`, costing `cost`, and drops the least recently used entries until
    /// both caps hold again.
    pub(crate) fn insert(&mut self, key: K, value: V, cost: usize) {
        self.clock += 1;
        if let Some(old) = self.entries.remove(&key) {
            self.cost -= old.cost;
        }
        if cost > self.max_cost {
            return;
        }
        self.entries.insert(
            key,
            Entry {
                value,
                cost,
                used: self.clock,
            },
        );
        self.cost += cost;
        while self.entries.len() > self.max_entries || self.cost > self.max_cost {
            let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.used)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            if let Some(gone) = self.entries.remove(&oldest) {
                self.cost -= gone.cost;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_least_recently_used_entry_goes_first() {
        let mut cache = Lru::new(2, 100);
        cache.insert("a", 1, 1);
        cache.insert("b", 2, 1);
        assert_eq!(cache.get(&"a"), Some(&1), "a is now the newer");
        cache.insert("c", 3, 1);
        assert_eq!(cache.len(), 2);
        assert!(cache.peek(&"b").is_none(), "b was the oldest");
        assert!(cache.peek(&"a").is_some() && cache.peek(&"c").is_some());
    }

    #[test]
    fn the_cost_cap_holds_and_a_value_dearer_than_it_is_not_kept() {
        let mut cache = Lru::new(100, 10);
        cache.insert(1, "x", 4);
        cache.insert(2, "y", 4);
        cache.insert(3, "z", 4);
        assert!(cache.cost() <= 10);
        assert_eq!(cache.len(), 2);
        assert!(cache.peek(&1).is_none());
        cache.insert(4, "huge", 11);
        assert!(cache.peek(&4).is_none());
        assert!(cache.cost() <= 10);
    }

    #[test]
    fn replacing_a_key_replaces_its_cost() {
        let mut cache = Lru::new(4, 10);
        cache.insert("k", 1, 8);
        cache.insert("k", 2, 3);
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.cost(), 3);
        assert_eq!(cache.get(&"k"), Some(&2));
    }

    #[test]
    fn a_thousand_inserts_never_pass_the_entry_cap() {
        let mut cache = Lru::new(16, usize::MAX);
        for n in 0..1000 {
            cache.insert(n, n, 1);
            assert!(cache.len() <= 16);
        }
    }
}
