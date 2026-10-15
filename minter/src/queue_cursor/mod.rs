//! Bounded rounds that rotate through a queue, so entries behind failing ones do not starve.

use std::{
    collections::BTreeMap,
    ops::Bound::{Excluded, Unbounded},
};

#[cfg(test)]
mod tests;

/// Where the previous round stopped. It lives in volatile memory, so the first round
/// after an upgrade starts from the first entry again.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct QueueCursor<K>(Option<K>);

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
