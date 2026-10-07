// Run without Cargo or ESP-IDF:
// rustc --edition=2021 -D warnings --test tests/camera_stop.rs -o target/camera_stop_tests
// ./target/camera_stop_tests
// These tests execute the production Rust algorithm against a modeled layout,
// not the SDK ABI, real spinlocks, or hardware interrupt/DMA behavior.
#[path = "../src/camera_stop.rs"]
mod camera_stop;

#[allow(non_camel_case_types, non_snake_case, non_upper_case_globals)]
pub mod fake_idf {
    use std::{cell::RefCell, ffi::c_void};

    pub type esp_err_t = i32;
    pub const ESP_IDF_VERSION_MAJOR: u32 = 6;
    pub const ESP_IDF_VERSION_MINOR: u32 = 1;
    pub const CONFIG_IDF_TARGET_ESP32P4: u32 = 1;
    pub const CONFIG_FREERTOS_NUMBER_OF_CORES: u32 = 2;
    pub const portMUX_FREE_VAL: u32 = 0xb33f_ffff;
    pub const portMUX_NO_TIMEOUT: i32 = -1;
    pub const pdTRUE: i32 = 1;
    pub const ESP_OK: esp_err_t = 0;
    pub const ESP_FAIL: esp_err_t = -1;
    pub const ESP_ERR_INVALID_ARG: esp_err_t = 0x102;
    pub const ESP_ERR_INVALID_STATE: esp_err_t = 0x103;
    pub const ESP_ERR_TIMEOUT: esp_err_t = 0x107;

    #[repr(C)]
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct esp_cam_ctlr_t {
        pub foreign_fields: [usize; 3],
    }
    pub type esp_cam_ctlr_handle_t = *mut esp_cam_ctlr_t;

    #[repr(C)]
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct dw_gdma_channel_t {
        pub foreign_fields: [u32; 4],
    }
    pub type dw_gdma_channel_handle_t = *mut dw_gdma_channel_t;

    #[repr(C)]
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct csi_controller_t {
        pub dummy_prefix: [usize; 2],
        pub dma_chan: dw_gdma_channel_handle_t,
        pub dummy_before_base: [u32; 3],
        pub base: esp_cam_ctlr_t,
        pub dummy_suffix: [usize; 4],
    }

    #[repr(C)]
    pub struct portMUX_TYPE {
        pub owner: u32,
        pub count: u32,
    }

    pub type dw_gdma_event_callback_t =
        Option<unsafe extern "C" fn(dw_gdma_channel_handle_t, *const c_void, *mut c_void) -> bool>;

    #[repr(C)]
    pub struct dw_gdma_event_callbacks_t {
        pub on_block_trans_done: dw_gdma_event_callback_t,
        pub on_full_trans_done: dw_gdma_event_callback_t,
        pub on_invalid_block: dw_gdma_event_callback_t,
    }

    #[derive(Clone, Copy)]
    pub(crate) struct Results {
        pub enter: i32,
        pub mask: esp_err_t,
        pub stop: esp_err_t,
    }

    impl Default for Results {
        fn default() -> Self {
            Self {
                enter: pdTRUE,
                mask: ESP_OK,
                stop: ESP_OK,
            }
        }
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    pub(crate) enum Step {
        Enter {
            lock: usize,
            owner: u32,
            count: u32,
            timeout: i32,
        },
        Mask {
            channel: usize,
            callbacks_none: [bool; 3],
            userdata: usize,
            held: bool,
        },
        Stop {
            handle: usize,
            held: bool,
        },
        Exit {
            lock: usize,
            owner: u32,
            count: u32,
            held: bool,
        },
    }

    #[derive(Default)]
    struct State {
        results: Results,
        steps: Vec<Step>,
        held: bool,
    }

    thread_local! {
        static STATE: RefCell<State> = RefCell::new(State::default());
    }

    pub(crate) fn reset(results: Results) {
        STATE.with(|state| {
            *state.borrow_mut() = State {
                results,
                ..State::default()
            }
        });
    }

    pub(crate) fn recorded() -> (Vec<Step>, bool) {
        STATE.with(|state| {
            let state = state.borrow();
            (state.steps.clone(), state.held)
        })
    }

    // Regular Rust functions allow host execution without pretending to provide
    // a foreign unwind boundary. No assertions or nested state borrows in fakes.
    pub unsafe fn xPortEnterCriticalTimeout(lock: *mut portMUX_TYPE, timeout: i32) -> i32 {
        STATE.with(|state| {
            let mut state = state.borrow_mut();
            state.steps.push(Step::Enter {
                lock: lock as usize,
                owner: (*lock).owner,
                count: (*lock).count,
                timeout,
            });
            let result = state.results.enter;
            if result == pdTRUE {
                state.held = true;
                (*lock).owner = 0;
                (*lock).count = 1;
            }
            result
        })
    }

    pub unsafe fn vPortExitCriticalMultiCore(lock: *mut portMUX_TYPE) {
        STATE.with(|state| {
            let mut state = state.borrow_mut();
            let held = state.held;
            state.steps.push(Step::Exit {
                lock: lock as usize,
                owner: (*lock).owner,
                count: (*lock).count,
                held,
            });
            state.held = false;
            (*lock).owner = portMUX_FREE_VAL;
            (*lock).count = 0;
        });
    }

    pub unsafe fn dw_gdma_channel_register_event_callbacks(
        channel: dw_gdma_channel_handle_t,
        callbacks: *mut dw_gdma_event_callbacks_t,
        userdata: *mut c_void,
    ) -> esp_err_t {
        STATE.with(|state| {
            let mut state = state.borrow_mut();
            let held = state.held;
            state.steps.push(Step::Mask {
                channel: channel as usize,
                callbacks_none: [
                    (*callbacks).on_block_trans_done.is_none(),
                    (*callbacks).on_full_trans_done.is_none(),
                    (*callbacks).on_invalid_block.is_none(),
                ],
                userdata: userdata as usize,
                held,
            });
            state.results.mask
        })
    }

    pub unsafe fn esp_cam_ctlr_stop(handle: esp_cam_ctlr_handle_t) -> esp_err_t {
        STATE.with(|state| {
            let mut state = state.borrow_mut();
            let held = state.held;
            state.steps.push(Step::Stop {
                handle: handle as usize,
                held,
            });
            state.results.stop
        })
    }
}

use fake_idf::{Results, Step};
use std::{cell::Cell, mem::offset_of, ptr, rc::Rc};

struct RetainedBuffer {
    bytes: Box<[u8; 64]>,
    drops: Rc<Cell<usize>>,
}

impl Drop for RetainedBuffer {
    fn drop(&mut self) {
        self.drops.set(self.drops.get() + 1);
    }
}

struct Caller {
    controller: Box<fake_idf::csi_controller_t>,
    channel: Box<fake_idf::dw_gdma_channel_t>,
    buffer: RetainedBuffer,
}

impl Caller {
    fn new() -> Self {
        let mut channel = Box::new(fake_idf::dw_gdma_channel_t {
            foreign_fields: [0x1234, 0x5678, 0x9abc, 0xdef0],
        });
        Self {
            controller: Box::new(fake_idf::csi_controller_t {
                dummy_prefix: [0x1111, 0x2222],
                dma_chan: ptr::from_mut(&mut *channel),
                dummy_before_base: [0x3333, 0x4444, 0x5555],
                base: fake_idf::esp_cam_ctlr_t {
                    foreign_fields: [0x6666, 0x7777, 0x8888],
                },
                dummy_suffix: [0x9999, 0xaaaa, 0xbbbb, 0xcccc],
            }),
            channel,
            buffer: RetainedBuffer {
                bytes: Box::new([0x5a; 64]),
                drops: Rc::new(Cell::new(0)),
            },
        }
    }

    fn handle(&mut self) -> fake_idf::esp_cam_ctlr_handle_t {
        ptr::addr_of_mut!(self.controller.base)
    }

    fn stop(&mut self) -> fake_idf::esp_err_t {
        let controller_before = *self.controller;
        let channel_before = *self.channel;
        let result = unsafe { camera_stop::stop(self.handle()) };
        // Compare fields, not uninitialized repr(C) padding bytes.
        assert_eq!(*self.controller, controller_before);
        assert_eq!(*self.channel, channel_before);
        assert_eq!(*self.buffer.bytes, [0x5a; 64]);
        assert_eq!(self.buffer.drops.get(), 0);
        result
    }
}

fn assert_steps(caller: &mut Caller, mask: bool, stop: bool, exit: bool) {
    let (steps, held) = fake_idf::recorded();
    assert!(!held, "critical section must not remain acquired");
    let lock = match steps.first() {
        Some(Step::Enter { lock, .. }) => *lock,
        other => panic!("expected critical entry, got {other:?}"),
    };
    assert_ne!(lock, 0);
    let mut expected = vec![Step::Enter {
        lock,
        owner: fake_idf::portMUX_FREE_VAL,
        count: 0,
        timeout: fake_idf::portMUX_NO_TIMEOUT,
    }];
    if mask {
        expected.push(Step::Mask {
            channel: caller.controller.dma_chan as usize,
            callbacks_none: [true; 3],
            userdata: 0,
            held: true,
        });
    }
    if stop {
        expected.push(Step::Stop {
            handle: caller.handle() as usize,
            held: true,
        });
    }
    if exit {
        expected.push(Step::Exit {
            lock,
            owner: 0,
            count: 1,
            held: true,
        });
    }
    assert_eq!(steps, expected);
}

#[test]
fn null_handle_is_rejected_without_entering_critical_section() {
    fake_idf::reset(Results::default());
    assert_eq!(
        unsafe { camera_stop::stop(ptr::null_mut()) },
        fake_idf::ESP_ERR_INVALID_ARG
    );
    assert_eq!(fake_idf::recorded(), (vec![], false));
}

#[test]
fn success_recovers_nonzero_offset_base_and_masks_actual_channel_before_stop() {
    fake_idf::reset(Results::default());
    let mut caller = Caller::new();
    let base_offset = offset_of!(fake_idf::csi_controller_t, base);
    assert!(base_offset > 0);
    assert!(offset_of!(fake_idf::csi_controller_t, dma_chan) < base_offset);
    assert_eq!(
        caller.handle() as usize - ptr::from_ref(&*caller.controller) as usize,
        base_offset,
    );
    assert_eq!(
        caller.controller.dma_chan,
        ptr::from_mut(&mut *caller.channel)
    );
    assert_eq!(caller.stop(), fake_idf::ESP_OK);
    assert_steps(&mut caller, true, true, true);
}

#[test]
fn masking_error_is_returned_without_stop_and_releases_critical_section() {
    fake_idf::reset(Results {
        mask: fake_idf::ESP_ERR_INVALID_STATE,
        ..Results::default()
    });
    let mut caller = Caller::new();
    assert_eq!(caller.stop(), fake_idf::ESP_ERR_INVALID_STATE);
    assert_steps(&mut caller, true, false, true);
}

#[test]
fn stop_error_is_returned_and_releases_critical_section() {
    fake_idf::reset(Results {
        stop: fake_idf::ESP_ERR_TIMEOUT,
        ..Results::default()
    });
    let mut caller = Caller::new();
    assert_eq!(caller.stop(), fake_idf::ESP_ERR_TIMEOUT);
    assert_steps(&mut caller, true, true, true);
}

#[test]
fn failed_critical_entry_skips_mask_stop_and_exit() {
    fake_idf::reset(Results {
        enter: 0,
        ..Results::default()
    });
    let mut caller = Caller::new();
    assert_eq!(caller.stop(), fake_idf::ESP_FAIL);
    assert_steps(&mut caller, false, false, false);
}

#[test]
fn nonzero_critical_result_other_than_pdtrue_is_not_acquisition() {
    fake_idf::reset(Results {
        enter: 2,
        ..Results::default()
    });
    let mut caller = Caller::new();
    assert_eq!(caller.stop(), fake_idf::ESP_FAIL);
    assert_steps(&mut caller, false, false, false);
}

#[test]
fn changed_channel_and_foreign_fields_are_read_without_overwriting_them() {
    fake_idf::reset(Results::default());
    let mut caller = Caller::new();
    let mut replacement = Box::new(fake_idf::dw_gdma_channel_t {
        foreign_fields: [42; 4],
    });
    caller.controller.dma_chan = ptr::from_mut(&mut *replacement);
    caller.controller.dummy_prefix = [usize::MAX, 0];
    caller.controller.dummy_before_base = [u32::MAX; 3];
    caller.controller.base.foreign_fields = [31, 37, 41];
    caller.controller.dummy_suffix = [43, 47, 53, 59];
    assert_ne!(
        caller.controller.dma_chan,
        ptr::from_mut(&mut *caller.channel)
    );
    assert_eq!(caller.stop(), fake_idf::ESP_OK);
    assert_steps(&mut caller, true, true, true);
    assert_eq!(replacement.foreign_fields, [42; 4]);
}

#[test]
fn modeled_missing_channel_propagates_registration_rejection_without_stop() {
    // A model mutation, not an assertion that this is a valid SDK controller.
    // The fake reports the registration error; production owns no channel validation.
    fake_idf::reset(Results {
        mask: fake_idf::ESP_ERR_INVALID_ARG,
        ..Results::default()
    });
    let mut caller = Caller::new();
    caller.controller.dma_chan = ptr::null_mut();
    assert_eq!(caller.stop(), fake_idf::ESP_ERR_INVALID_ARG);
    assert_steps(&mut caller, true, false, true);
}

#[test]
fn caller_retains_allocations_on_every_failure_until_explicit_caller_cleanup() {
    for (results, error, mask, stop, exit) in [
        (
            Results {
                enter: 0,
                ..Results::default()
            },
            fake_idf::ESP_FAIL,
            false,
            false,
            false,
        ),
        (
            Results {
                mask: fake_idf::ESP_ERR_INVALID_STATE,
                ..Results::default()
            },
            fake_idf::ESP_ERR_INVALID_STATE,
            true,
            false,
            true,
        ),
        (
            Results {
                stop: fake_idf::ESP_ERR_TIMEOUT,
                ..Results::default()
            },
            fake_idf::ESP_ERR_TIMEOUT,
            true,
            true,
            true,
        ),
    ] {
        fake_idf::reset(results);
        let mut caller = Caller::new();
        let drops = Rc::clone(&caller.buffer.drops);
        assert_eq!(caller.stop(), error);
        assert_steps(&mut caller, mask, stop, exit);
        assert_eq!(drops.get(), 0);
        // Neither production stop nor the fake owns these allocations. In a real
        // caller, cleanup must wait for an independently established DMA/ISR quiescence.
        drop(caller);
        assert_eq!(drops.get(), 1);
    }
}
