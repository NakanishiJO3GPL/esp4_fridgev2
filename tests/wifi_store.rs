// Run on the development host without ESP-IDF:
// rustc --edition=2021 --test tests/wifi_store.rs -o /tmp/esp4_fridgev2_wifi_store_tests
// /tmp/esp4_fridgev2_wifi_store_tests
#[path = "../src/wifi_store.rs"]
mod wifi_store;

use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Barrier,
};
use std::thread;
use wifi_store::{Budget, Latest, Reservation};

#[test]
fn trigger_notification_is_independent_of_jpeg_publication() {
    let mut latest = Latest::new(0xabc);
    assert_eq!(latest.trigger_header(), "0000000000000abc:0:0");
    latest.record_trigger(42);
    assert!(latest.snapshot().is_none());
    assert_eq!(latest.trigger_header(), "0000000000000abc:1:42");
    assert_eq!(latest.publish(42, "jpeg"), 1);
    assert_eq!(latest.trigger_header(), "0000000000000abc:1:42");
    // A busy/skipped trigger still changes notification, not the JPEG identity.
    latest.record_trigger(43);
    assert_eq!(latest.trigger_header(), "0000000000000abc:2:43");
    let image = latest.snapshot().unwrap();
    assert_eq!(image.version, 1);
    assert_eq!(image.frame_sequence, 42);
    assert_eq!(image.data, "jpeg");
    let rebooted = Latest::<&str>::new(0xdef);
    assert_eq!(rebooted.trigger_header(), "0000000000000def:0:0");
}

#[test]
fn budget_is_bounded_and_released_on_drop() {
    let budget = Budget::new(10);
    let clone = budget.clone();
    let first = budget.reserve(6).unwrap();
    let second = clone.reserve(4).unwrap();
    assert_eq!(budget.used(), 10);
    assert_eq!(clone.used(), 10);
    assert!(budget.reserve(1).is_none());
    assert_eq!(budget.used(), 10);
    drop(first);
    assert_eq!(clone.used(), 4);
    let replacement = clone.reserve(6).unwrap();
    drop(second);
    assert_eq!(budget.used(), 6);
    drop(replacement);
    assert_eq!(budget.used(), 0);
    assert!(budget.reserve(11).is_none());
    assert_eq!(budget.used(), 0);
}

#[test]
fn budget_rejects_arithmetic_overflow_without_changing_usage() {
    let budget = Budget::new(usize::MAX);
    let full = budget.reserve(usize::MAX).unwrap();
    assert!(budget.reserve(1).is_none());
    assert!(budget.reserve(usize::MAX).is_none());
    assert_eq!(budget.used(), usize::MAX);
    drop(full);
    assert_eq!(budget.used(), 0);
    let small = budget.reserve(1).unwrap();
    assert!(budget.reserve(usize::MAX).is_none());
    assert_eq!(budget.used(), 1);
    drop(small);
    assert_eq!(budget.used(), 0);
}

#[test]
fn zero_byte_reservations_work_even_at_the_limit() {
    let budget = Budget::new(0);
    let empty = budget.reserve(0).unwrap();
    assert!(budget.reserve(1).is_none());
    drop(empty);
    assert_eq!(budget.used(), 0);
    let budget = Budget::new(usize::MAX);
    let full = budget.reserve(usize::MAX).unwrap();
    drop(budget.reserve(0).unwrap());
    assert_eq!(budget.used(), usize::MAX);
    drop(full);
    assert_eq!(budget.used(), 0);
}

#[test]
fn reservation_can_outlive_the_budget_handle() {
    let budget = Budget::new(8);
    let observer = budget.clone();
    let reservation = budget.reserve(8).unwrap();
    drop(budget);
    assert_eq!(observer.used(), 8);
    drop(reservation);
    assert_eq!(observer.used(), 0);
    let reservation = observer.reserve(8).unwrap();
    drop(observer);
    drop(reservation);
}

#[test]
fn concurrent_reservations_never_exceed_the_limit() {
    const WORKERS: usize = 32;
    const LIMIT: usize = 8;
    let budget = Budget::new(LIMIT);
    let start = Arc::new(Barrier::new(WORKERS + 1));
    let held = Arc::new(Barrier::new(WORKERS + 1));
    let release = Arc::new(Barrier::new(WORKERS + 1));
    let successes = Arc::new(AtomicUsize::new(0));
    let threads: Vec<_> = (0..WORKERS)
        .map(|_| {
            let budget = budget.clone();
            let start = Arc::clone(&start);
            let held = Arc::clone(&held);
            let release = Arc::clone(&release);
            let successes = Arc::clone(&successes);
            thread::spawn(move || {
                start.wait();
                let reservation = budget.reserve(1);
                if reservation.is_some() {
                    successes.fetch_add(1, Ordering::Relaxed);
                }
                held.wait();
                release.wait();
                drop(reservation);
            })
        })
        .collect();
    start.wait();
    held.wait();
    let used = budget.used();
    let count = successes.load(Ordering::Relaxed);
    let rejected = budget.reserve(1).is_none();
    release.wait();
    for thread in threads {
        thread.join().unwrap();
    }
    assert_eq!(used, LIMIT);
    assert_eq!(count, LIMIT);
    assert!(rejected);
    assert_eq!(budget.used(), 0);
    drop(budget.reserve(LIMIT).unwrap());
    assert_eq!(budget.used(), 0);
}

#[test]
fn old_snapshots_remain_unchanged_across_replacement_and_store_drop() {
    let mut latest = Latest::new(0xabc);
    assert_eq!(latest.boot_id, 0xabc);
    assert!(latest.snapshot().is_none());
    assert_eq!(latest.publish(42, vec![1, 2, 3]), 1);
    let old = latest.snapshot().unwrap();
    let old_clone = latest.snapshot().unwrap();
    assert!(Arc::ptr_eq(&old, &old_clone));
    assert_eq!(latest.publish(43, vec![4, 5]), 2);
    let current = latest.snapshot().unwrap();
    assert!(!Arc::ptr_eq(&old, &current));
    drop(latest);
    assert_eq!(old.version, 1);
    assert_eq!(old.frame_sequence, 42);
    assert_eq!(old.data, [1, 2, 3]);
    assert_eq!(current.version, 2);
    assert_eq!(current.frame_sequence, 43);
    assert_eq!(current.data, [4, 5]);
}

#[test]
fn repeated_frame_sequences_still_publish_new_versions() {
    let mut latest = Latest::new(1);
    assert_eq!(latest.publish(7, "first"), 1);
    let first = latest.snapshot().unwrap();
    assert_eq!(latest.publish(7, "second"), 2);
    let second = latest.snapshot().unwrap();
    assert_eq!(first.frame_sequence, second.frame_sequence);
    assert_eq!(first.version, 1);
    assert_eq!(second.version, 2);
    assert_eq!(first.data, "first");
    assert_eq!(second.data, "second");
}

#[test]
fn reboot_resets_versions_with_a_different_boot_id() {
    let mut before = Latest::new(0xabc);
    before.publish(90, "old");
    before.publish(91, "latest old");
    let old = before.snapshot().unwrap();
    let mut after = Latest::new(0xdef);
    assert_ne!(before.boot_id, after.boot_id);
    assert!(after.snapshot().is_none());
    assert_eq!(after.publish(0, "new"), 1);
    let new = after.snapshot().unwrap();
    assert_eq!(new.version, 1);
    assert_eq!(new.frame_sequence, 0);
    assert_eq!(old.version, 2);
    assert_eq!(old.data, "latest old");
}

#[test]
fn snapshot_payload_keeps_its_reservation_until_the_last_owner_drops() {
    struct Allocation {
        data: Vec<u8>,
        _reservation: Reservation,
    }

    let budget = Budget::new(10);
    let mut latest = Latest::new(1);
    latest.publish(
        1,
        Allocation {
            data: vec![1; 6],
            _reservation: budget.reserve(6).unwrap(),
        },
    );
    let old = latest.snapshot().unwrap();
    let old_clone = Arc::clone(&old);
    latest.publish(
        2,
        Allocation {
            data: vec![2; 4],
            _reservation: budget.reserve(4).unwrap(),
        },
    );
    assert_eq!(old.data.data, [1; 6]);
    assert_eq!(budget.used(), 10);
    assert!(budget.reserve(1).is_none());
    drop(old);
    assert_eq!(budget.used(), 10);
    drop(old_clone);
    assert_eq!(budget.used(), 4);
    let current = latest.snapshot().unwrap();
    drop(latest);
    assert_eq!(budget.used(), 4);
    drop(current);
    assert_eq!(budget.used(), 0);
}
