//! Bounded rounds that rotate through a queue, so entries behind failing ones do not starve.

use std::{
    cell::RefCell,
    collections::BTreeMap,
    ops::Bound::{Excluded, Unbounded},
};

#[cfg(test)]
mod tests;

/// Where the previous round stopped. It lives in volatile memory, so the first round
/// after an upgrade starts from the first entry again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QueueCursor<K>(Option<K>);

impl<K> Default for QueueCursor<K> {
    fn default() -> Self {
        Self(None)
    }
}

impl<K: Copy + Ord> QueueCursor<K> {
    /// Selects at most `limit` entries after the cursor, wrapping once, each at most once.
    pub fn round<'a, V>(&self, queue: &'a BTreeMap<K, V>, limit: usize) -> Vec<(&'a K, &'a V)> {
        let start = self.0.map_or(Unbounded, Excluded);
        queue
            .range((start, Unbounded))
            .chain(queue.iter())
            .take(queue.len().min(limit))
            .collect()
    }

    pub fn advance(&mut self, key: K) {
        self.0 = Some(key);
    }
}

/// The cursors of a family of queues, one per queue.
pub struct QueueCursors<Q, K>(RefCell<BTreeMap<Q, QueueCursor<K>>>);

impl<Q: Copy + Ord, K: Copy + Ord> QueueCursors<Q, K> {
    pub const fn new() -> Self {
        Self(RefCell::new(BTreeMap::new()))
    }

    /// Selects at most `limit` entries of the queue after its cursor, wrapping once.
    pub fn round<'a, V>(
        &self,
        queue: Q,
        entries: &'a BTreeMap<K, V>,
        limit: usize,
    ) -> Vec<(&'a K, &'a V)> {
        self.0
            .borrow()
            .get(&queue)
            .copied()
            .unwrap_or_default()
            .round(entries, limit)
    }

    pub fn advance(&self, queue: Q, key: K) {
        self.0.borrow_mut().entry(queue).or_default().advance(key);
    }

    /// Starts the next round of the queue from its first entry.
    pub fn clear(&self, queue: Q) {
        self.0.borrow_mut().remove(&queue);
    }

    pub fn reset(&self) {
        self.0.borrow_mut().clear();
    }
}
