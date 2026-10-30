//! `SIGPROF` handlers that unwind the interrupted thread into a ring buffer, one per [`Layout`].
//!
//! [`configure`] once, install [`handler`] for the running CPython's layout, and drain with
//! [`consumer`]. Each record is the frames of the interrupted thread, innermost first.

use super::{Frame, Layout, Unwinder};
use crate::signal_sampler::ringbuffer::{Consumer, Record, RingBuffer};
use core::ffi::c_void;
use core::sync::atomic::{AtomicUsize, Ordering};

const SLOTS: usize = 1 << 16;

/// `PyGILState_GetThisThreadState`: the thread state of the calling thread, or 0.
pub type ThreadStateGetter = extern "C" fn() -> usize;

pub type Handler = extern "C" fn(libc::c_int, *mut libc::siginfo_t, *mut c_void);

struct Global {
    records: RingBuffer<Frame, SLOTS>,
    code_type: AtomicUsize,
    thread_state: AtomicUsize,
    max_frames: AtomicUsize,
    dropped: AtomicUsize,
}

static GLOBAL: Global = Global {
    records: RingBuffer::new(),
    code_type: AtomicUsize::new(0),
    thread_state: AtomicUsize::new(0),
    max_frames: AtomicUsize::new(0),
    dropped: AtomicUsize::new(0),
};

/// Sets what the handlers need. Until it is called they do nothing.
///
/// `code_type` is the address of `PyCode_Type`. `max_frames` bounds each walk.
pub fn configure(code_type: usize, thread_state: ThreadStateGetter, max_frames: usize) {
    GLOBAL.max_frames.store(max_frames, Ordering::Relaxed);
    GLOBAL
        .thread_state
        .store(thread_state as usize, Ordering::Relaxed);
    GLOBAL.code_type.store(code_type, Ordering::Release);
}

/// The handler for CPython with layout `L`.
pub fn handler<L: Layout>() -> Handler {
    on_signal::<L>
}

/// Reads the records the handlers wrote. `None` if another consumer is active.
pub fn consumer() -> Option<Consumer<'static, Frame, SLOTS>> {
    GLOBAL.records.consumer()
}

/// The number of samples the handlers gave up on since the last call: the buffer was full or
/// busy, or a walk failed.
pub fn take_dropped() -> usize {
    GLOBAL.dropped.swap(0, Ordering::Relaxed)
}

extern "C" fn on_signal<L: Layout>(_: libc::c_int, _: *mut libc::siginfo_t, _: *mut c_void) {
    let g = &GLOBAL;
    let code_type = g.code_type.load(Ordering::Acquire);
    let thread_state = g.thread_state.load(Ordering::Relaxed);
    if code_type == 0 || thread_state == 0 {
        return;
    }
    // SAFETY: `configure` stored a `ThreadStateGetter`.
    let thread_state = unsafe { core::mem::transmute::<usize, ThreadStateGetter>(thread_state) };
    let tstate = thread_state();
    if tstate == 0 {
        return;
    }
    let Some(mut producer) = g.records.producer() else {
        g.dropped.fetch_add(1, Ordering::Relaxed);
        return;
    };
    let record = producer.record();
    let max_frames = g.max_frames.load(Ordering::Relaxed);
    if !record_stack::<L>(code_type, max_frames, tstate, record) {
        g.dropped.fetch_add(1, Ordering::Relaxed);
    }
}

/// Commits the record if the whole walk succeeded, and drops it otherwise.
fn record_stack<L: Layout>(
    code_type: usize,
    max_frames: usize,
    tstate: usize,
    mut record: Record<'_, '_, Frame, SLOTS>,
) -> bool {
    Unwinder::<L>::new(code_type, max_frames)
        .unwind(tstate, &mut record)
        .is_ok()
        && record.commit().is_ok()
}
