use signal_ringbuffer::RingBuffer;
use signal_unwind::{Error, Frame, Unwinder, V3_14};

static RB: RingBuffer<Frame, 16> = RingBuffer::new();

#[test]
fn unreadable_address_fails_and_keeps_errno() {
    signal_unwind::init().unwrap();
    let unwinder = Unwinder::<V3_14>::new(0, 8);
    let mut p = RB.producer().unwrap();
    let mut record = p.record();
    // SAFETY: errno_ptr returns the calling thread's errno.
    let errno = unsafe { errno_ptr() };
    unsafe { *errno = 239 };
    assert_eq!(unwinder.unwind(0x1000, &mut record), Err(Error::Read));
    assert_eq!(
        unwinder.unwind(usize::MAX - 3, &mut record),
        Err(Error::Invalid)
    );
    assert_eq!(unsafe { *errno }, 239);
}

#[cfg(target_os = "linux")]
unsafe fn errno_ptr() -> *mut libc::c_int {
    unsafe { libc::__errno_location() }
}

#[cfg(target_os = "macos")]
unsafe fn errno_ptr() -> *mut libc::c_int {
    unsafe { libc::__error() }
}
