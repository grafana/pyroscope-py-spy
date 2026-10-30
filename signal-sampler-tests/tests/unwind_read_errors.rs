use py_spy::signal_sampler::ringbuffer::{PopError, RingBuffer};
use py_spy::signal_sampler::unwind::{Error, Frame, Unwinder, V3_14};

static RB: RingBuffer<Frame, 16> = RingBuffer::new();

#[test]
fn unreadable_address_fails_and_keeps_errno() {
    py_spy::signal_sampler::unwind::init().unwrap();
    // SAFETY: errno_ptr returns the calling thread's errno.
    let errno = unsafe { errno_ptr() };
    unsafe { *errno = 239 };
    let unwind = |thread_state| Unwinder::<V3_14>::new(0, thread_state, 8).unwind(&RB);
    assert_eq!(unwind(unmapped_thread_state), Err(Error::Read));
    assert_eq!(unwind(overflowing_thread_state), Err(Error::Invalid));
    assert_eq!(unwind(no_thread_state), Err(Error::NoThreadState));
    assert_eq!(unsafe { *errno }, 239);
    let mut c = RB.consumer().unwrap();
    assert_eq!(
        c.pop_slice(&mut [Frame::default(); 16]),
        Err(PopError::Empty)
    );
}

extern "C" fn unmapped_thread_state() -> usize {
    0x1000
}

extern "C" fn overflowing_thread_state() -> usize {
    usize::MAX - 3
}

extern "C" fn no_thread_state() -> usize {
    0
}

#[cfg(target_os = "linux")]
unsafe fn errno_ptr() -> *mut libc::c_int {
    unsafe { libc::__errno_location() }
}

#[cfg(target_os = "macos")]
unsafe fn errno_ptr() -> *mut libc::c_int {
    unsafe { libc::__error() }
}
