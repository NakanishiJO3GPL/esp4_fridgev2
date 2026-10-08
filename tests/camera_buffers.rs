// Run on the development host without ESP-IDF:
// rustc --edition=2021 --test tests/camera_buffers.rs -o target/camera_buffers_tests
// ./target/camera_buffers_tests
#[path = "../src/camera/buffers.rs"]
mod buffers;

use buffers::{Buffers, FrameLease, LeasePool};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

struct TestPool {
    buffers: Buffers,
    data: [Box<[u8]>; 2],
    drops: Arc<AtomicUsize>,
}

impl LeasePool for TestPool {
    fn buffers(&self) -> &Buffers {
        &self.buffers
    }
}

impl Drop for TestPool {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

fn completed_pool() -> (Arc<TestPool>, Arc<AtomicUsize>) {
    let drops = Arc::new(AtomicUsize::new(0));
    let pool = Arc::new(TestPool {
        buffers: Buffers::new(),
        data: [
            vec![1, 2, 3, 4].into_boxed_slice(),
            vec![5, 6, 7, 8].into_boxed_slice(),
        ],
        drops: Arc::clone(&drops),
    });
    assert_eq!(pool.buffers.next_dma(), 0);
    assert_eq!(pool.buffers.next_dma(), 1);
    assert!(pool.buffers.finish(0, true));
    (pool, drops)
}

#[test]
fn shared_lease_releases_only_after_borrowed_and_all_owned_guards_drop() {
    let (pool, _) = completed_pool();
    let borrowed = FrameLease::acquire(&pool).unwrap();
    let owned = Arc::clone(&borrowed);
    let another_owned = Arc::clone(&borrowed);
    assert_eq!(borrowed.index(), 0);
    assert_eq!(borrowed.sequence(), 1);
    assert_eq!(
        borrowed.pool().data[0].as_ptr(),
        owned.pool().data[0].as_ptr()
    );
    drop(borrowed);
    drop(owned);
    for _ in 0..100 {
        assert_eq!(pool.buffers.next_dma(), 1);
        assert!(!pool.buffers.finish(1, true));
        assert!(FrameLease::acquire(&pool).is_none());
    }
    assert_eq!(another_owned.sequence(), 1);
    assert_eq!(&*another_owned.pool().data[0], &[1, 2, 3, 4]);
    drop(another_owned);
    assert_eq!(pool.buffers.next_dma(), 0);
    assert!(pool.buffers.finish(1, true));
    let next = FrameLease::acquire(&pool).unwrap();
    assert_eq!(next.index(), 1);
    assert_eq!(next.sequence(), 102);
}

#[test]
fn dropping_owned_before_borrowed_does_not_release_buffer() {
    let (pool, _) = completed_pool();
    let borrowed = FrameLease::acquire(&pool).unwrap();
    drop(Arc::clone(&borrowed));
    assert_eq!(pool.buffers.next_dma(), 1);
    assert!(!pool.buffers.finish(1, true));
    assert_eq!(&*borrowed.pool().data[0], &[1, 2, 3, 4]);
    drop(borrowed);
    assert_eq!(pool.buffers.next_dma(), 0);
}

#[test]
fn owned_lease_retains_pool_after_camera_owner_drops_and_moves_to_thread() {
    let (pool, drops) = completed_pool();
    let weak_pool = Arc::downgrade(&pool);
    let borrowed = FrameLease::acquire(&pool).unwrap();
    let owned = Arc::clone(&borrowed);
    drop(borrowed);
    // Models the camera dropping its pool reference after quiescing CSI.
    drop(pool);
    assert!(weak_pool.upgrade().is_some());
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    std::thread::spawn(move || {
        assert_eq!(owned.sequence(), 1);
        assert_eq!(&*owned.pool().data[owned.index()], &[1, 2, 3, 4]);
        drop(owned);
    })
    .join()
    .unwrap();
    assert!(weak_pool.upgrade().is_none());
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

#[test]
fn concurrent_lease_drops_do_not_release_while_borrowed_guard_remains() {
    let (pool, _) = completed_pool();
    let borrowed = FrameLease::acquire(&pool).unwrap();
    let workers: Vec<_> = (0..8)
        .map(|_| {
            let owned = Arc::clone(&borrowed);
            std::thread::spawn(move || drop(owned))
        })
        .collect();
    for worker in workers {
        worker.join().unwrap();
    }
    assert_eq!(pool.buffers.next_dma(), 1);
    assert!(!pool.buffers.finish(1, true));
    drop(borrowed);
    assert_eq!(pool.buffers.next_dma(), 0);
}
