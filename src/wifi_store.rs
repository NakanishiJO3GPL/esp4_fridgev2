//! Byte-budget accounting and immutable snapshots, independent of ESP-IDF.

use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

#[derive(Clone)]
pub(super) struct Budget {
    used: Arc<AtomicUsize>,
    limit: usize,
}

impl Budget {
    pub(super) fn new(limit: usize) -> Self {
        Self {
            used: Arc::new(AtomicUsize::new(0)),
            limit,
        }
    }

    pub(super) fn reserve(&self, bytes: usize) -> Option<Reservation> {
        let mut used = self.used.load(Ordering::Relaxed);
        loop {
            let next = used.checked_add(bytes)?;
            if next > self.limit {
                return None;
            }
            // This counter accounts for bytes; it does not publish allocation data.
            match self
                .used
                .compare_exchange(used, next, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => {
                    return Some(Reservation {
                        used: Arc::clone(&self.used),
                        bytes,
                    });
                }
                Err(actual) => used = actual,
            }
        }
    }

    #[cfg(test)]
    pub(super) fn used(&self) -> usize {
        self.used.load(Ordering::Relaxed)
    }
}

/// Keep this token with the allocation until its memory has been freed.
pub(super) struct Reservation {
    used: Arc<AtomicUsize>,
    bytes: usize,
}

impl Drop for Reservation {
    fn drop(&mut self) {
        self.used.fetch_sub(self.bytes, Ordering::Relaxed);
    }
}

pub(super) struct Versioned<T> {
    pub version: u32,
    pub frame_sequence: u32,
    pub data: T,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct TriggerEvent {
    pub sequence: u32,
    pub frame_sequence: u32,
}

/// Callers synchronize access externally; snapshots outlive subsequent publishes.
pub(super) struct Latest<T> {
    pub boot_id: u64,
    version: u32,
    current: Option<Arc<Versioned<T>>>,
    trigger: TriggerEvent,
}

impl<T> Latest<T> {
    pub(super) fn new(boot_id: u64) -> Self {
        Self {
            boot_id,
            version: 0,
            current: None,
            trigger: TriggerEvent {
                sequence: 0,
                frame_sequence: 0,
            },
        }
    }

    pub(super) fn record_trigger(&mut self, frame_sequence: u32) {
        let sequence = self.trigger.sequence.wrapping_add(1);
        self.trigger = TriggerEvent {
            sequence: if sequence == 0 { 1 } else { sequence },
            frame_sequence,
        };
    }

    pub(super) fn trigger_header(&self) -> String {
        format!(
            "{:016x}:{}:{}",
            self.boot_id, self.trigger.sequence, self.trigger.frame_sequence
        )
    }

    pub(super) fn publish(&mut self, frame_sequence: u32, data: T) -> u32 {
        self.version = self.version.wrapping_add(1);
        if self.version == 0 {
            self.version = 1;
        }
        self.current = Some(Arc::new(Versioned {
            version: self.version,
            frame_sequence,
            data,
        }));
        self.version
    }

    pub(super) fn snapshot(&self) -> Option<Arc<Versioned<T>>> {
        self.current.as_ref().map(Arc::clone)
    }
}

#[cfg(test)]
mod tests {
    use super::Latest;

    #[test]
    fn trigger_wrap_skips_no_event_sentinel() {
        let mut latest = Latest::<()>::new(1);
        latest.trigger.sequence = u32::MAX;
        latest.record_trigger(0);
        assert_eq!(latest.trigger_header(), "0000000000000001:1:0");
    }

    #[test]
    fn version_wrap_skips_zero() {
        let mut latest = Latest::new(1);
        latest.version = u32::MAX - 1;
        assert_eq!(latest.publish(10, "before wrap"), u32::MAX);
        let old = latest.snapshot().unwrap();
        assert_eq!(latest.publish(11, "after wrap"), 1);
        let current = latest.snapshot().unwrap();
        assert_eq!(current.version, 1);
        assert_eq!(current.frame_sequence, 11);
        assert_eq!(current.data, "after wrap");
        assert_eq!(old.version, u32::MAX);
        assert_eq!(old.data, "before wrap");
        assert_eq!(latest.publish(12, "next"), 2);
    }
}
