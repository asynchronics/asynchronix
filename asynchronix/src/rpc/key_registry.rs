use crate::time::{ActionKey, MonotonicTime};
use crate::util::indexed_priority_queue::{IndexedPriorityQueue, InsertKey};

pub(crate) type KeyRegistryId = InsertKey;

/// A collection of `ActionKey`s indexed by a unique identifier.
#[derive(Default)]
pub(crate) struct KeyRegistry {
    keys: IndexedPriorityQueue<MonotonicTime, ActionKey>,
}

impl KeyRegistry {
    /// Inserts an `ActionKey` into the registry.
    ///
    /// The provided expiration deadline is the latest time at which the key may
    /// still be active.
    pub(crate) fn insert_key(
        &mut self,
        action_key: ActionKey,
        expiration: MonotonicTime,
    ) -> KeyRegistryId {
        self.keys.insert(expiration, action_key)
    }

    /// Inserts a non-expiring `ActionKey` into the registry.
    pub(crate) fn insert_eternal_key(&mut self, action_key: ActionKey) -> KeyRegistryId {
        self.keys.insert(MonotonicTime::MAX, action_key)
    }

    /// Removes an `ActionKey` from the registry and returns it.
    ///
    /// Returns `None` if the key was not found in the registry.
    pub(crate) fn extract_key(&mut self, key_id: KeyRegistryId) -> Option<ActionKey> {
        self.keys.extract(key_id).map(|(_, key)| key)
    }

    /// Remove keys with an expiration deadline strictly predating the argument.
    pub(crate) fn remove_expired_keys(&mut self, now: MonotonicTime) {
        while let Some(expiration) = self.keys.peek_key() {
            if *expiration >= now {
                return;
            }

            self.keys.pull();
        }
    }
}
