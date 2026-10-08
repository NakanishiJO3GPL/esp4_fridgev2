// Run on the development host without ESP-IDF:
// rustc --edition=2021 --test tests/wifi_nvs.rs -o /tmp/esp4_fridgev2_wifi_nvs_tests
// /tmp/esp4_fridgev2_wifi_nvs_tests
#[path = "../src/wifi/nvs.rs"]
mod wifi_nvs;

use std::io;
use wifi_nvs::{next_boot_id_with, Driver};

const HANDLE: u32 = 17;
const ERROR_CODE: i32 = 0x1103; // ESP_ERR_NVS_TYPE_MISMATCH: not a missing key.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Operation {
    Open,
    Get,
    Set(u64),
    Commit,
    Close,
}

struct Fake {
    stored: Option<u64>,
    pending: Option<u64>,
    fail: Option<Operation>,
    operations: Vec<Operation>,
    open: bool,
}

impl Fake {
    fn new(stored: Option<u64>) -> Self {
        Self {
            stored,
            pending: None,
            fail: None,
            operations: Vec::new(),
            open: false,
        }
    }

    fn record(&mut self, operation: Operation) -> io::Result<()> {
        self.operations.push(operation);
        if self.fail == Some(operation) {
            Err(io::Error::from_raw_os_error(ERROR_CODE))
        } else {
            Ok(())
        }
    }

    fn check_handle(&self, handle: u32) {
        assert_eq!(handle, HANDLE);
        assert!(self.open, "operation requires an open handle");
    }
}

impl Driver for Fake {
    type Handle = u32;

    fn open(&mut self) -> io::Result<u32> {
        assert!(!self.open);
        self.record(Operation::Open)?;
        self.open = true;
        Ok(HANDLE)
    }

    fn get_boot(&mut self, handle: u32) -> io::Result<Option<u64>> {
        self.check_handle(handle);
        self.record(Operation::Get)?;
        Ok(self.stored)
    }

    fn set_boot(&mut self, handle: u32, value: u64) -> io::Result<()> {
        self.check_handle(handle);
        self.record(Operation::Set(value))?;
        self.pending = Some(value);
        Ok(())
    }

    fn commit(&mut self, handle: u32) -> io::Result<()> {
        self.check_handle(handle);
        self.record(Operation::Commit)?;
        self.stored = self.pending.take();
        Ok(())
    }

    fn close(&mut self, handle: u32) {
        self.check_handle(handle);
        self.operations.push(Operation::Close);
        self.open = false;
        self.pending = None;
    }
}

fn assert_success(initial: Option<u64>, expected: u64) {
    let mut fake = Fake::new(initial);
    assert_eq!(next_boot_id_with(&mut fake).unwrap(), expected);
    assert_eq!(fake.stored, Some(expected));
    assert_eq!(fake.pending, None);
    assert!(!fake.open);
    assert_eq!(
        fake.operations,
        [
            Operation::Open,
            Operation::Get,
            Operation::Set(expected),
            Operation::Commit,
            Operation::Close,
        ]
    );
}

#[test]
fn missing_key_starts_at_zero_and_commits_one() {
    assert_success(None, 1);
}

#[test]
fn existing_counter_increments() {
    assert_success(Some(41), 42);
}

#[test]
fn existing_zero_increments() {
    assert_success(Some(0), 1);
}

#[test]
fn last_available_id_is_valid() {
    assert_success(Some(u64::MAX - 1), u64::MAX);
}

#[test]
fn overflow_closes_without_set_or_commit() {
    let mut fake = Fake::new(Some(u64::MAX));
    let error = next_boot_id_with(&mut fake).unwrap_err();
    assert!(error.to_string().contains("ESP_ERR_INVALID_STATE"));
    assert_eq!(fake.stored, Some(u64::MAX));
    assert!(!fake.open);
    assert_eq!(
        fake.operations,
        [Operation::Open, Operation::Get, Operation::Close]
    );
}

fn assert_failure(operation: Operation, expected_operations: &[Operation]) {
    let mut fake = Fake::new(Some(41));
    fake.fail = Some(operation);
    let error = next_boot_id_with(&mut fake).unwrap_err();
    assert_eq!(error.raw_os_error(), Some(ERROR_CODE));
    assert_eq!(fake.stored, Some(41));
    assert_eq!(fake.pending, None);
    assert!(!fake.open);
    assert_eq!(fake.operations, expected_operations);
}

#[test]
fn open_error_returns_without_close_or_retry() {
    assert_failure(Operation::Open, &[Operation::Open]);
}

#[test]
fn invalid_existing_read_returns_error_without_write_and_closes() {
    assert_failure(
        Operation::Get,
        &[Operation::Open, Operation::Get, Operation::Close],
    );
}

#[test]
fn set_error_closes_without_commit() {
    assert_failure(
        Operation::Set(42),
        &[
            Operation::Open,
            Operation::Get,
            Operation::Set(42),
            Operation::Close,
        ],
    );
}

#[test]
fn commit_error_closes_and_does_not_return_new_id() {
    assert_failure(
        Operation::Commit,
        &[
            Operation::Open,
            Operation::Get,
            Operation::Set(42),
            Operation::Commit,
            Operation::Close,
        ],
    );
}

#[test]
fn committed_id_is_used_by_next_open() {
    let mut fake = Fake::new(None);
    assert_eq!(next_boot_id_with(&mut fake).unwrap(), 1);
    assert_eq!(next_boot_id_with(&mut fake).unwrap(), 2);
    assert_eq!(fake.stored, Some(2));
    assert_eq!(
        fake.operations
            .iter()
            .filter(|operation| **operation == Operation::Close)
            .count(),
        2
    );
    assert!(!fake.open);
}

#[test]
fn implementation_never_initializes_or_erases_nvs() {
    // The fake has no erase operation; also check the target-only FFI adapter.
    let source = include_str!("../src/wifi/nvs.rs");
    assert!(!source.contains("nvs_flash_"));
    assert!(!source.contains("nvs_erase"));
}
