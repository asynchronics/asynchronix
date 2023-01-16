//! Associative priority queue.

#![allow(unused)]

use std::mem;

/// An associative container optimized for extraction of the value with the
/// lowest key and deletion of arbitrary key-value pairs.
///
/// This implementation has the same theoretical complexity for insert and pull
/// operations as a conventional array-based binary heap but does differ from
/// the latter in some important aspects:
///
/// - elements can be deleted in *O*(log(*N*)) time rather than *O*(*N*) time
///   using a unique index returned at insertion time.
/// - same-key elements are guaranteed to be pulled in FIFO order,
///
/// Under the hood, the priority queue relies on a binary heap cross-indexed
/// with values stored in a slab allocator. Each item of the binary heap
/// contains an index pointing to the associated slab-allocated node, as well as
/// the user-provided key. Each slab node contains the value associated to the
/// key and a back-pointing index to the binary heap. The heap items also
/// contain a unique epoch which allows same-key nodes to be sorted by insertion
/// order. The epoch is used as well to build unique indices that enable
/// efficient deletion of arbitrary key-value pairs.
///
/// The slab-based design is what makes *O*(log(*N*)) deletion possible, but it
/// does come with some trade-offs:
///
/// - its memory footprint is higher because it needs 2 extra pointer-sized
///   indices for each element to cross-index the heap and the slab,
/// - its computational footprint is higher because of the extra cost associated
///   with random slab access; that being said, array-based binary heaps are not
///   extremely cache-friendly to start with so unless the slab becomes very
///   fragmented, this is not expected to introduce more than a reasonable
///   constant-factor penalty compared to a conventional binary heap.
///
/// The computational penalty is partially offset by the fact that the value
/// never needs to be moved from the moment it is inserted until it is pulled.
///
/// Note that the `Copy` bound on they keys could be lifted but this would make
/// the implementation slightly less efficient unless `unsafe` is used.
pub(crate) struct PriorityQueue<K, V>
where
    K: Copy + Clone + Ord,
{
    heap: Vec<Item<K>>,
    slab: Vec<Node<V>>,
    first_free_node: Option<usize>,
    next_epoch: u64,
}

impl<K: Copy + Ord, V> PriorityQueue<K, V> {
    /// Creates an empty `PriorityQueue`.
    pub(crate) fn new() -> Self {
        Self {
            heap: Vec::new(),
            slab: Vec::new(),
            first_free_node: None,
            next_epoch: 0,
        }
    }

    /// Creates an empty `PriorityQueue` with at least the specified capacity.
    pub(crate) fn with_capacity(capacity: usize) -> Self {
        Self {
            heap: Vec::with_capacity(capacity),
            slab: Vec::with_capacity(capacity),
            first_free_node: None,
            next_epoch: 0,
        }
    }

    /// Returns the number of key-value pairs in the priority queue.
    pub(crate) fn len(&self) -> usize {
        self.heap.len()
    }

    /// Inserts a new key-value pair and returns a unique insertion key.
    ///
    /// This operation has *O*(log(*N*)) amortized worse-case theoretical
    /// complexity and *O*(1) amortized theoretical complexity for a
    /// sufficiently random heap.
    pub(crate) fn insert(&mut self, key: K, value: V) -> InsertKey {
        // Build a unique key from the user-provided key and a unique epoch.
        let epoch = self.next_epoch;
        assert_ne!(epoch, u64::MAX);
        self.next_epoch += 1;
        let unique_key = UniqueKey { key, epoch };

        // Add a new node to the slab, either by re-using a free node or by
        // appending a new one.
        let slab_idx = match self.first_free_node {
            Some(idx) => {
                self.first_free_node = self.slab[idx].unwrap_next_free_node();

                self.slab[idx] = Node::HeapNode(HeapNode {
                    value,
                    heap_idx: 0, // temporary value overridden in `sift_up`
                });

                idx
            }
            None => {
                let idx = self.slab.len();
                self.slab.push(Node::HeapNode(HeapNode {
                    value,
                    heap_idx: 0, // temporary value overridden in `sift_up`
                }));

                idx
            }
        };

        // Add a new node at the bottom of the heap.
        let heap_idx = self.heap.len();
        self.heap.push(Item {
            key: unique_key, // temporary value overridden in `sift_up`
            slab_idx: 0,     // temporary value overridden in `sift_up`
        });

        // Sift up the new node.
        self.sift_up(
            Item {
                key: unique_key,
                slab_idx,
            },
            heap_idx,
        );

        InsertKey { slab_idx, epoch }
    }

    /// Pulls the value with the lowest key.
    ///
    /// If there are several equal lowest keys, the value which was inserted
    /// first is returned.
    ///
    /// This operation has *O*(log(N)) non-amortized theoretical complexity.
    pub(crate) fn pull(&mut self) -> Option<(K, V)> {
        let item = self.heap.first()?;
        let top_slab_idx = item.slab_idx;
        let key = item.key.key;

        // Free the top node, extracting its value.
        let value = mem::replace(
            &mut self.slab[top_slab_idx],
            Node::FreeNode(FreeNode {
                next: self.first_free_node,
            }),
        )
        .unwrap_value();

        self.first_free_node = Some(top_slab_idx);

        // Sift the last node at the bottom of the heap from the top of the heap.
        let last_item = self.heap.pop().unwrap();
        if last_item.slab_idx != top_slab_idx {
            self.sift_down(last_item, 0);
        }

        Some((key, value))
    }

    /// Peeks a reference to the key-value pair with the lowest key, leaving it
    /// in the queue.
    ///
    /// If there are several equal lowest keys, a reference to the key-value
    /// pair which was inserted first is returned.
    ///
    /// This operation has *O*(1) non-amortized theoretical complexity.
    pub(crate) fn peek(&self) -> Option<(&K, &V)> {
        let item = self.heap.first()?;
        let top_slab_idx = item.slab_idx;
        let key = &item.key.key;
        let value = self.slab[top_slab_idx].unwrap_value_ref();

        Some((key, value))
    }

    /// Peeks a reference to the lowest key, leaving it in the queue.
    ///
    /// If there are several equal lowest keys, a reference to the key which was
    /// inserted first is returned.
    ///
    /// This operation has *O*(1) non-amortized theoretical complexity.
    pub(crate) fn peek_key(&self) -> Option<&K> {
        let item = self.heap.first()?;

        Some(&item.key.key)
    }

    /// Delete the key-value pair associated to the provided insertion key if it
    /// is still in the queue.
    ///
    /// Using an insertion key returned from another `PriorityQueue` is a logic
    /// error and could result in the deletion of an arbitrary key-value pair.
    ///
    /// This method returns `true` if the pair was indeed in the queue and
    /// `false` otherwise.
    ///
    /// This operation has guaranteed *O*(log(*N*)) theoretical complexity.
    pub(crate) fn delete(&mut self, insert_key: InsertKey) -> bool {
        // Check that (i) there is a node at this index, (ii) this node is in
        // the heap and (iii) this node has the correct epoch.
        let slab_idx = insert_key.slab_idx;
        let heap_idx = if let Some(Node::HeapNode(node)) = self.slab.get(slab_idx) {
            let heap_idx = node.heap_idx;
            if self.heap[heap_idx].key.epoch != insert_key.epoch {
                return false;
            }
            heap_idx
        } else {
            return false;
        };

        // If the last item of the heap is not the one to be deleted, sift it up
        // or down as appropriate starting from the vacant spot.
        let last_item = self.heap.pop().unwrap();
        if let Some(item) = self.heap.get(heap_idx) {
            if last_item.key < item.key {
                self.sift_up(last_item, heap_idx);
            } else {
                self.sift_down(last_item, heap_idx);
            }
        }

        // Free the deleted node in the slab.
        self.slab[slab_idx] = Node::FreeNode(FreeNode {
            next: self.first_free_node,
        });
        self.first_free_node = Some(slab_idx);

        true
    }

    /// Take a heap item and, starting at `heap_idx`, move it up the heap while
    /// a parent has a larger key.
    #[inline]
    fn sift_up(&mut self, item: Item<K>, heap_idx: usize) {
        let mut child_heap_idx = heap_idx;
        let key = &item.key;

        while child_heap_idx != 0 {
            let parent_heap_idx = (child_heap_idx - 1) / 2;

            // Stop when the key is larger or equal to the parent's.
            if key >= &self.heap[parent_heap_idx].key {
                break;
            }

            // Move the parent down one level.
            self.heap[child_heap_idx] = self.heap[parent_heap_idx];
            let parent_slab_idx = self.heap[parent_heap_idx].slab_idx;
            *self.slab[parent_slab_idx].unwrap_heap_index_mut() = child_heap_idx;

            // Stop when the key is larger or equal to the parent's.
            if key >= &self.heap[parent_heap_idx].key {
                break;
            }
            // Make the former parent the new child.
            child_heap_idx = parent_heap_idx;
        }

        // Move the original item to the current child.
        self.heap[child_heap_idx] = item;
        *self.slab[item.slab_idx].unwrap_heap_index_mut() = child_heap_idx;
    }

    /// Take a heap item and, starting at `heap_idx`, move it down the heap
    /// while a child has a smaller key.
    #[inline]
    fn sift_down(&mut self, item: Item<K>, heap_idx: usize) {
        let mut parent_heap_idx = heap_idx;
        let mut child_heap_idx = 2 * parent_heap_idx + 1;
        let key = &item.key;

        while child_heap_idx < self.heap.len() {
            // If the sibling exists and has a smaller key, make it the
            // candidate for swapping.
            if let Some(other_child) = self.heap.get(child_heap_idx + 1) {
                child_heap_idx += (self.heap[child_heap_idx].key > other_child.key) as usize;
            }

            // Stop when the key is smaller or equal to the child with the smallest key.
            if key <= &self.heap[child_heap_idx].key {
                break;
            }

            // Move the child up one level.
            self.heap[parent_heap_idx] = self.heap[child_heap_idx];
            let child_slab_idx = self.heap[child_heap_idx].slab_idx;
            *self.slab[child_slab_idx].unwrap_heap_index_mut() = parent_heap_idx;

            // Make the child the new parent.
            parent_heap_idx = child_heap_idx;
            child_heap_idx = 2 * parent_heap_idx + 1;
        }

        // Move the original item to the current parent.
        self.heap[parent_heap_idx] = item;
        *self.slab[item.slab_idx].unwrap_heap_index_mut() = parent_heap_idx;
    }
}

/// Data related to a single key-value pair stored in the heap.
#[derive(Copy, Clone)]
struct Item<K: Copy> {
    // A unique key by which the heap is sorted.
    key: UniqueKey<K>,
    // An index pointing to the corresponding node in the slab.
    slab_idx: usize,
}

/// Data related to a single key-value pair stored in the slab.
enum Node<V> {
    FreeNode(FreeNode),
    HeapNode(HeapNode<V>),
}

impl<V> Node<V> {
    /// Unwraps the `FreeNode::next` field.
    fn unwrap_next_free_node(&self) -> Option<usize> {
        match self {
            Self::FreeNode(n) => n.next,
            _ => panic!("the node was expected to be a free node"),
        }
    }

    /// Unwraps the `HeapNode::value` field.
    fn unwrap_value(self) -> V {
        match self {
            Self::HeapNode(n) => n.value,
            _ => panic!("the node was expected to be a heap node"),
        }
    }

    /// Unwraps the `HeapNode::value` field.
    fn unwrap_value_ref(&self) -> &V {
        match self {
            Self::HeapNode(n) => &n.value,
            _ => panic!("the node was expected to be a heap node"),
        }
    }

    /// Unwraps a mutable reference to the `HeapNode::heap_idx` field.
    fn unwrap_heap_index_mut(&mut self) -> &mut usize {
        match self {
            Self::HeapNode(n) => &mut n.heap_idx,
            _ => panic!("the node was expected to be a heap node"),
        }
    }
}

/// A node that is no longer in the binary heap.
struct FreeNode {
    // An index pointing to the next free node, if any.
    next: Option<usize>,
}

/// A node currently in the binary heap.
struct HeapNode<V> {
    // The value associated to this node.
    value: V,
    // Index of the node in the heap.
    heap_idx: usize,
}

/// A unique insertion key that can be used for key-value pair deletion.
#[derive(Copy, Clone, Debug, Hash, PartialEq, Eq)]
pub(crate) struct InsertKey {
    // An index pointing to a node in the slab.
    slab_idx: usize,
    // The epoch when the node was inserted.
    epoch: u64,
}

/// A unique key made of the user-provided key complemented by a unique epoch.
///
/// Implementation note: `UniqueKey` automatically derives `PartialOrd`, which
/// implies that lexicographic order between `key` and `epoch` must be preserved
/// to make sure that `key` has a higher sorting priority than `epoch`.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct UniqueKey<K: Copy + Clone> {
    /// The user-provided key.
    key: K,
    /// A unique epoch that indicates the insertion date.
    epoch: u64,
}

#[cfg(all(test, not(asynchronix_loom)))]
mod tests {
    use std::fmt::Debug;

    use super::*;

    enum Op<K, V> {
        Insert(K, V),
        InsertAndMark(K, V),
        Pull(Option<(K, V)>),
        DeleteMarked(bool),
    }

    fn check<K: Copy + Clone + Ord + Debug, V: Eq + Debug>(
        operations: impl Iterator<Item = Op<K, V>>,
    ) {
        let mut queue = PriorityQueue::new();
        let mut marked = None;

        for op in operations {
            match op {
                Op::Insert(key, value) => {
                    queue.insert(key, value);
                }
                Op::InsertAndMark(key, value) => {
                    marked = Some(queue.insert(key, value));
                }
                Op::Pull(kv) => {
                    assert_eq!(queue.pull(), kv);
                }
                Op::DeleteMarked(success) => {
                    assert_eq!(
                        queue.delete(marked.take().expect("no item was marked for deletion")),
                        success
                    )
                }
            }
        }
    }

    #[test]
    fn priority_queue_smoke() {
        let operations = [
            Op::Insert(5, 'a'),
            Op::Insert(2, 'b'),
            Op::Insert(3, 'c'),
            Op::Insert(4, 'd'),
            Op::Insert(9, 'e'),
            Op::Insert(1, 'f'),
            Op::Insert(8, 'g'),
            Op::Insert(0, 'h'),
            Op::Insert(7, 'i'),
            Op::Insert(6, 'j'),
            Op::Pull(Some((0, 'h'))),
            Op::Pull(Some((1, 'f'))),
            Op::Pull(Some((2, 'b'))),
            Op::Pull(Some((3, 'c'))),
            Op::Pull(Some((4, 'd'))),
            Op::Pull(Some((5, 'a'))),
            Op::Pull(Some((6, 'j'))),
            Op::Pull(Some((7, 'i'))),
            Op::Pull(Some((8, 'g'))),
            Op::Pull(Some((9, 'e'))),
        ];

        check(operations.into_iter());
    }

    #[test]
    fn priority_queue_interleaved() {
        let operations = [
            Op::Insert(2, 'a'),
            Op::Insert(7, 'b'),
            Op::Insert(5, 'c'),
            Op::Pull(Some((2, 'a'))),
            Op::Insert(4, 'd'),
            Op::Pull(Some((4, 'd'))),
            Op::Insert(8, 'e'),
            Op::Insert(2, 'f'),
            Op::Pull(Some((2, 'f'))),
            Op::Pull(Some((5, 'c'))),
            Op::Pull(Some((7, 'b'))),
            Op::Insert(5, 'g'),
            Op::Insert(3, 'h'),
            Op::Pull(Some((3, 'h'))),
            Op::Pull(Some((5, 'g'))),
            Op::Pull(Some((8, 'e'))),
            Op::Pull(None),
        ];

        check(operations.into_iter());
    }

    #[test]
    fn priority_queue_equal_keys() {
        let operations = [
            Op::Insert(4, 'a'),
            Op::Insert(1, 'b'),
            Op::Insert(3, 'c'),
            Op::Pull(Some((1, 'b'))),
            Op::Insert(4, 'd'),
            Op::Insert(8, 'e'),
            Op::Insert(3, 'f'),
            Op::Pull(Some((3, 'c'))),
            Op::Pull(Some((3, 'f'))),
            Op::Pull(Some((4, 'a'))),
            Op::Insert(8, 'g'),
            Op::Pull(Some((4, 'd'))),
            Op::Pull(Some((8, 'e'))),
            Op::Pull(Some((8, 'g'))),
            Op::Pull(None),
        ];

        check(operations.into_iter());
    }

    #[test]
    fn priority_queue_delete_valid() {
        let operations = [
            Op::Insert(8, 'a'),
            Op::Insert(1, 'b'),
            Op::Insert(3, 'c'),
            Op::InsertAndMark(3, 'd'),
            Op::Insert(2, 'e'),
            Op::Pull(Some((1, 'b'))),
            Op::Insert(4, 'f'),
            Op::DeleteMarked(true),
            Op::Insert(5, 'g'),
            Op::Pull(Some((2, 'e'))),
            Op::Pull(Some((3, 'c'))),
            Op::Pull(Some((4, 'f'))),
            Op::Pull(Some((5, 'g'))),
            Op::Pull(Some((8, 'a'))),
            Op::Pull(None),
        ];

        check(operations.into_iter());
    }

    #[test]
    fn priority_queue_delete_invalid() {
        let operations = [
            Op::Insert(0, 'a'),
            Op::Insert(7, 'b'),
            Op::InsertAndMark(2, 'c'),
            Op::Insert(4, 'd'),
            Op::Pull(Some((0, 'a'))),
            Op::Insert(2, 'e'),
            Op::Pull(Some((2, 'c'))),
            Op::Insert(4, 'f'),
            Op::DeleteMarked(false),
            Op::Pull(Some((2, 'e'))),
            Op::Pull(Some((4, 'd'))),
            Op::Pull(Some((4, 'f'))),
            Op::Pull(Some((7, 'b'))),
            Op::Pull(None),
        ];

        check(operations.into_iter());
    }

    #[test]
    fn priority_queue_fuzz() {
        use std::cell::Cell;
        use std::collections::BTreeMap;

        use crate::util::rng::Rng;

        // Number of fuzzing operations.
        const ITER: usize = if cfg!(miri) { 1000 } else { 10_000_000 };

        // Inclusive upper bound for randomly generated keys.
        const MAX_KEY: u64 = 99;

        // Probabilistic weight of each of the 4 operations.
        //
        // The weight for pull values should probably stay close to the sum of
        // the two insertion weights to prevent queue size runaway.
        const INSERT_WEIGHT: u64 = 5;
        const INSERT_AND_MARK_WEIGHT: u64 = 1;
        const PULL_WEIGHT: u64 = INSERT_WEIGHT + INSERT_AND_MARK_WEIGHT;
        const DELETE_MARKED_WEIGHT: u64 = 1;

        // Defines 4 basic operations on the priority queue, each of them being
        // performed on both the tested implementation and on a shadow queue
        // implemented with a `BTreeMap`. Any mismatch between the outcomes of
        // pull and delete operations between the two queues triggers a panic.
        let epoch: Cell<usize> = Cell::new(0);
        let marked: Cell<Option<InsertKey>> = Cell::new(None);
        let shadow_marked: Cell<Option<(u64, usize)>> = Cell::new(None);

        let insert_fn = |queue: &mut PriorityQueue<u64, u64>,
                         shadow_queue: &mut BTreeMap<(u64, usize), u64>,
                         key,
                         value| {
            queue.insert(key, value);
            shadow_queue.insert((key, epoch.get()), value);
            epoch.set(epoch.get() + 1);
        };

        let insert_and_mark_fn = |queue: &mut PriorityQueue<u64, u64>,
                                  shadow_queue: &mut BTreeMap<(u64, usize), u64>,
                                  key,
                                  value| {
            marked.set(Some(queue.insert(key, value)));
            shadow_queue.insert((key, epoch.get()), value);
            shadow_marked.set(Some((key, epoch.get())));
            epoch.set(epoch.get() + 1);
        };

        let pull_fn = |queue: &mut PriorityQueue<u64, u64>,
                       shadow_queue: &mut BTreeMap<(u64, usize), u64>| {
            let value = queue.pull();
            let shadow_value = match shadow_queue.iter().next() {
                Some((&unique_key, &value)) => {
                    shadow_queue.remove(&unique_key);
                    Some((unique_key.0, value))
                }
                None => None,
            };
            assert_eq!(value, shadow_value);
        };

        let delete_marked_fn =
            |queue: &mut PriorityQueue<u64, u64>,
             shadow_queue: &mut BTreeMap<(u64, usize), u64>| {
                let success = match marked.take() {
                    Some(delete_key) => Some(queue.delete(delete_key)),
                    None => None,
                };
                let shadow_success = match shadow_marked.take() {
                    Some(delete_key) => Some(shadow_queue.remove(&delete_key).is_some()),
                    None => None,
                };
                assert_eq!(success, shadow_success);
            };

        // Fuzz away.
        let mut queue = PriorityQueue::new();
        let mut shadow_queue = BTreeMap::new();

        let rng = Rng::new(12345);
        const TOTAL_WEIGHT: u64 =
            INSERT_WEIGHT + INSERT_AND_MARK_WEIGHT + PULL_WEIGHT + DELETE_MARKED_WEIGHT;

        for _ in 0..ITER {
            // Randomly choose one of the 4 possible operations, respecting the
            // probability weights.
            let mut op = rng.gen_bounded(TOTAL_WEIGHT);
            if op < INSERT_WEIGHT {
                let key = rng.gen_bounded(MAX_KEY + 1);
                let val = rng.gen();
                insert_fn(&mut queue, &mut shadow_queue, key, val);
                continue;
            }
            op -= INSERT_WEIGHT;
            if op < INSERT_AND_MARK_WEIGHT {
                let key = rng.gen_bounded(MAX_KEY + 1);
                let val = rng.gen();
                insert_and_mark_fn(&mut queue, &mut shadow_queue, key, val);
                continue;
            }
            op -= INSERT_AND_MARK_WEIGHT;
            if op < PULL_WEIGHT {
                pull_fn(&mut queue, &mut shadow_queue);
                continue;
            }
            delete_marked_fn(&mut queue, &mut shadow_queue);
        }
    }
}
