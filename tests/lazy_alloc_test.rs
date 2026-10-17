use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use py_spy::config::{LineNo, LockingStrategy};
use py_spy::lazy_stack_trace::SampleBuffers;
use py_spy::{Config, PythonSpy};

struct CountingAlloc;

thread_local! {
    static ALLOCATIONS: Cell<usize> = const { Cell::new(0) };
}

unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.with(|count| count.set(count.get() + 1));
        System.alloc(layout)
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout)
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCATIONS.with(|count| count.set(count.get() + 1));
        System.realloc(ptr, layout, new_size)
    }
}

#[global_allocator]
static GLOBAL: CountingAlloc = CountingAlloc;

#[test]
fn test_lazy_sample_does_not_allocate() {
    #[cfg(target_os = "macos")]
    {
        // We need root permissions here to run this on OSX
        if unsafe { libc::geteuid() } != 0 {
            return;
        }
    }
    let mut child = std::process::Command::new("python")
        .arg("./tests/scripts/busyloop.py")
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(400));

    let config = Config {
        blocking: LockingStrategy::NonBlocking,
        gil_only: true,
        lineno: LineNo::LastInstruction,
        ..Default::default()
    };
    let mut spy = PythonSpy::retry_new(child.id() as _, &config, 20).unwrap();
    let mut bufs = SampleBuffers::default();
    let (mut name, mut filename) = (String::with_capacity(256), String::with_capacity(4096));

    let mut sample_once = |spy: &mut PythonSpy, bufs: &mut SampleBuffers| -> usize {
        let before = ALLOCATIONS.with(Cell::get);
        let mut frames = 0;
        let sample = spy.sample(bufs).unwrap();
        for trace in sample.traces() {
            for frame in trace.frames() {
                frame.name(&mut name).unwrap();
                frame.filename(&mut filename).unwrap();
                frames += 1;
            }
        }
        assert!(frames > 0 || sample.is_empty());

        let mut walker = spy.walk_threads(bufs).unwrap();
        while let Some(thread) = walker.next_thread().unwrap() {
            for frame in thread.unwind().unwrap().frames() {
                frame.name(&mut name).unwrap();
                frame.filename(&mut filename).unwrap();
            }
        }
        ALLOCATIONS.with(Cell::get) - before
    };

    for _ in 0..10 {
        sample_once(&mut spy, &mut bufs);
    }
    let allocations: usize = (0..100).map(|_| sample_once(&mut spy, &mut bufs)).sum();
    child.kill().unwrap();
    child.wait().unwrap();
    assert_eq!(allocations, 0);
}
