use super::QueueCursor;
use proptest::{collection::btree_set, option, prelude::*};
use std::collections::{BTreeMap, BTreeSet};

fn queue(keys: impl IntoIterator<Item = u64>) -> BTreeMap<u64, ()> {
    keys.into_iter().map(|key| (key, ())).collect()
}

fn keys(round: Vec<(&u64, &())>) -> Vec<u64> {
    round.into_iter().map(|(key, _)| *key).collect()
}

#[test]
fn should_start_from_the_first_entry_without_a_cursor() {
    let queue = queue([1, 2, 3, 4, 5]);
    let cursor = QueueCursor::default();

    assert_eq!(keys(cursor.round(&queue, 3)), [1, 2, 3]);
    assert_eq!(keys(cursor.round(&queue, 10)), [1, 2, 3, 4, 5]);
}

#[test]
fn should_continue_after_the_cursor_and_wrap_once() {
    let queue = queue([1, 2, 3, 4, 5]);
    let mut cursor = QueueCursor::default();

    cursor.advance(3);
    assert_eq!(keys(cursor.round(&queue, 3)), [4, 5, 1]);
    assert_eq!(keys(cursor.round(&queue, 10)), [4, 5, 1, 2, 3]);

    cursor.advance(5);
    assert_eq!(keys(cursor.round(&queue, 2)), [1, 2]);
}

#[test]
fn should_continue_after_a_removed_cursor_key() {
    let queue = queue([1, 2, 4, 5]);
    let mut cursor = QueueCursor::default();
    cursor.advance(3);

    assert_eq!(keys(cursor.round(&queue, 2)), [4, 5]);
}

#[test]
fn should_select_nothing_from_an_empty_queue() {
    let mut cursor = QueueCursor::default();
    cursor.advance(7);

    assert!(cursor.round(&queue([]), 3).is_empty());
}

proptest! {
    #[test]
    fn should_visit_every_key_within_one_rotation(
        keys in btree_set(0u64..100, 0..30),
        limit in 1usize..8,
        start in option::of(0u64..100),
    ) {
        let queue: BTreeMap<u64, ()> = keys.iter().map(|key| (*key, ())).collect();
        let mut cursor = QueueCursor::default();
        if let Some(start) = start {
            cursor.advance(start);
        }
        let mut seen = BTreeSet::new();
        for _ in 0..queue.len().div_ceil(limit) {
            let round: Vec<u64> = cursor
                .round(&queue, limit)
                .into_iter()
                .map(|(key, _)| *key)
                .collect();
            prop_assert!(round.len() <= limit.min(queue.len()));
            prop_assert_eq!(round.iter().collect::<BTreeSet<_>>().len(), round.len());
            if let Some(last) = round.last() {
                cursor.advance(*last);
            }
            seen.extend(round);
        }
        prop_assert_eq!(seen, keys);
    }
}
