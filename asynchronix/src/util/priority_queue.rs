//! Associative priority queue.

use std::cmp::Ordering;
use std::collections::BinaryHeap;

/// A key-value pair ordered by keys in inverse order, with epoch-based ordering
/// for equal keys.
struct Item<K, V>
where
    K: Ord,
{
    key: K,
    value: V,
    epoch: u64,
}

impl<K, V> Ord for Item<K, V>
where
    K: Ord,
{
    fn cmp(&self, other: &Self) -> Ordering {
        self.key
            .cmp(&other.key)
            .then_with(|| self.epoch.cmp(&other.epoch))
            .reverse()
    }
}

impl<K, V> PartialOrd for Item<K, V>
where
    K: Ord,
{
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl<K, V> Eq for Item<K, V> where K: Ord {}

impl<K, V> PartialEq for Item<K, V>
where
    K: Ord,
{
    fn eq(&self, other: &Self) -> bool {
        (self.key == other.key) && (self.epoch == other.epoch)
    }
}

/// An associative container optimized for extraction of the key-value pair with
/// the lowest key, based on a binary heap.
///
/// The insertion order of equal keys is preserved, with FIFO ordering.
pub(crate) struct PriorityQueue<K, V>
where
    K: Ord,
{
    heap: BinaryHeap<Item<K, V>>,
    next_epoch: u64,
}

impl<K: Copy + Ord, V> PriorityQueue<K, V> {
    /// Creates an empty `PriorityQueue`.
    pub(crate) fn new() -> Self {
        Self {
            heap: BinaryHeap::new(),
            next_epoch: 0,
        }
    }

    /// Inserts a new key-value pair.
    ///
    /// This operation has *O*(log(*N*)) amortized worse-case theoretical
    /// complexity and *O*(1) amortized theoretical complexity for a
    /// sufficiently random heap.
    pub(crate) fn insert(&mut self, key: K, value: V) {
        // Build an element from the user-provided key-value and a unique epoch.
        let epoch = self.next_epoch;
        assert_ne!(epoch, u64::MAX);
        self.next_epoch += 1;
        let item = Item { key, value, epoch };
        self.heap.push(item);
    }

    /// Pulls the value with the lowest key.
    ///
    /// If there are several equal lowest keys, the value which was inserted
    /// first is returned.
    ///
    /// This operation has *O*(log(N)) non-amortized theoretical complexity.
    pub(crate) fn pull(&mut self) -> Option<(K, V)> {
        let Item { key, value, .. } = self.heap.pop()?;

        Some((key, value))
    }

    /// Peeks a reference to the key-value pair with the lowest key, leaving it
    /// in the queue.
    ///
    /// If there are several equal lowest keys, references to the key-value pair
    /// which was inserted first is returned.
    ///
    /// This operation has *O*(1) non-amortized theoretical complexity.
    pub(crate) fn peek(&self) -> Option<(&K, &V)> {
        let Item {
            ref key, ref value, ..
        } = self.heap.peek()?;

        Some((key, value))
    }
}

#[cfg(all(test, not(asynchronix_loom)))]
mod tests {
    use super::*;

    #[test]
    fn priority_smoke() {
        let mut q = PriorityQueue::new();

        q.insert(5, 'e');
        q.insert(2, 'y');
        q.insert(1, 'a');
        q.insert(3, 'c');
        q.insert(2, 'z');
        q.insert(4, 'd');
        q.insert(2, 'x');

        assert_eq!(q.peek(), Some((&1, &'a')));
        assert_eq!(q.pull(), Some((1, 'a')));
        assert_eq!(q.peek(), Some((&2, &'y')));
        assert_eq!(q.pull(), Some((2, 'y')));
        assert_eq!(q.peek(), Some((&2, &'z')));
        assert_eq!(q.pull(), Some((2, 'z')));
        assert_eq!(q.peek(), Some((&2, &'x')));
        assert_eq!(q.pull(), Some((2, 'x')));
        assert_eq!(q.peek(), Some((&3, &'c')));
        assert_eq!(q.pull(), Some((3, 'c')));
        assert_eq!(q.peek(), Some((&4, &'d')));
        assert_eq!(q.pull(), Some((4, 'd')));
        assert_eq!(q.peek(), Some((&5, &'e')));
        assert_eq!(q.pull(), Some((5, 'e')));
    }
}
