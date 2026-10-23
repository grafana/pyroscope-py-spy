//! Unwinds the CPython that pyo3 links, which must be 3.14: build with `PYO3_PYTHON=python3.14`.

use pyo3::ffi;
use pyo3::prelude::*;
use pyo3::types::PyDict;
use signal_ringbuffer::RingBuffer;
use signal_timer::{c_int, siginfo_t, Mode, SignalHandler, Timer};
use signal_unwind::{Checked, Direct, Error, Frame, Unwinder, V3_14};
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::OnceLock;
use std::time::Duration;

const MAX_FRAMES: usize = 128;

static CODE_TYPE: OnceLock<usize> = OnceLock::new();
static RB: RingBuffer<Frame, { 1 << 16 }> = RingBuffer::new();
static CHECKED: AtomicBool = AtomicBool::new(true);
static NO_TSTATE: AtomicUsize = AtomicUsize::new(0);
static BUSY: AtomicUsize = AtomicUsize::new(0);
static ERRORS: [AtomicUsize; 4] = [const { AtomicUsize::new(0) }; 4];

extern "C" fn on_signal(_: c_int, _: *mut siginfo_t, _: *mut c_void) {
    let Some(&code_type) = CODE_TYPE.get() else {
        return;
    };
    let tstate = unsafe { ffi::PyThreadState_GetUnchecked() } as usize;
    if tstate == 0 {
        NO_TSTATE.fetch_add(1, Ordering::Relaxed);
        return;
    }
    let Some(mut p) = RB.producer() else {
        BUSY.fetch_add(1, Ordering::Relaxed);
        return;
    };
    let mut record = p.record();
    let result = if CHECKED.load(Ordering::Relaxed) {
        Unwinder::<V3_14, _>::new(Checked::new(), code_type, MAX_FRAMES).unwind(tstate, &mut record)
    } else {
        let direct = unsafe { Direct::new() };
        Unwinder::<V3_14, _>::new(direct, code_type, MAX_FRAMES).unwind(tstate, &mut record)
    };
    match result {
        Ok(_) => {
            if record.commit().is_err() {
                ERRORS[Error::Full as usize].fetch_add(1, Ordering::Relaxed);
            }
        }
        Err(e) => {
            ERRORS[e as usize].fetch_add(1, Ordering::Relaxed);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Resolved {
    name: String,
    line: i32,
}

struct Py<'py> {
    py: Python<'py>,
    globals: Bound<'py, PyDict>,
}

impl<'py> Py<'py> {
    /// Runs `src` and returns its code object, which keeps the recorded frames resolvable.
    fn run(&self, src: &str) -> Bound<'py, PyAny> {
        let builtins = self.py.import("builtins").unwrap();
        let code = builtins
            .getattr("compile")
            .and_then(|compile| compile.call1((src, "scenario.py", "exec")))
            .unwrap();
        builtins
            .getattr("exec")
            .and_then(|exec| exec.call1((&code, &self.globals)))
            .unwrap();
        code
    }

    /// Resolves every pending record, dropping `<shim>` frames.
    fn drain(&self) -> Vec<Vec<Resolved>> {
        let mut c = RB.consumer().unwrap();
        let mut buf = [Frame::default(); MAX_FRAMES];
        let mut stacks = Vec::new();
        while let Ok(len) = c.pop_slice(&mut buf) {
            let stack = buf[..len]
                .iter()
                .filter_map(|f| {
                    let code =
                        unsafe { Bound::from_borrowed_ptr(self.py, f.code as *mut ffi::PyObject) };
                    let attr = |name| code.getattr(name).unwrap().extract::<String>().unwrap();
                    if attr("co_filename") == "<shim>" {
                        return None;
                    }
                    let line = unsafe {
                        ffi::PyCode_Addr2Line(
                            f.code as *mut ffi::PyCodeObject,
                            f.instr_offset as c_int,
                        )
                    };
                    Some(Resolved {
                        name: attr("co_qualname"),
                        line,
                    })
                })
                .collect();
            stacks.push(stack);
        }
        stacks
    }
}

const PRELUDE: &str = "\
import signal, threading
def trigger():
    signal.pthread_kill(threading.get_ident(), signal.SIGPROF)  #> trigger
";

const CALLS: &str = "\
def c():
    trigger()  #> c
def b():
    c()  #> b
def a():
    b()  #> a
a()  #> <module>
";

const ENTRY_FRAMES: &str = "\
class Foo:
    def __init__(self):
        trigger()  #> Foo.__init__
def gen():
    yield Foo()  #> gen
def key(x):
    return next(gen())  #> key
list(map(key, [1]))  #> <module>
";

const THREAD: &str = "\
def worker():
    trigger()  #> worker
t = threading.Thread(target=worker)
t.start()
t.join()
";

const DEEP: &str = "\
def deep(n):
    return trigger() if n == 0 else deep(n - 1)
deep(200)
";

const SPIN: &str = "\
import time
def spin(end):
    x = 0
    while time.process_time() < end:
        x += 1
    return x
def outer(end):
    return spin(end)
end = time.process_time() + 0.2
while time.process_time() < end:
    outer(end)
";

/// The expected frame for each name, at the line marked `#> name` in `src`.
fn expect(src: &str, names: &[&str]) -> Vec<Resolved> {
    names
        .iter()
        .map(|name| {
            let marker = format!("#> {name}");
            let line = src
                .lines()
                .position(|l| l.ends_with(&marker))
                .unwrap_or_else(|| panic!("no marker for {name}"));
            Resolved {
                name: name.to_string(),
                line: line as i32 + 1,
            }
        })
        .collect()
}

fn names(stack: &[Resolved]) -> Vec<&str> {
    stack.iter().map(|f| f.name.as_str()).collect()
}

fn errors() -> [usize; 4] {
    ERRORS.each_ref().map(|e| e.swap(0, Ordering::Relaxed))
}

fn check_triggered(py: &Py, scenario: &str) {
    let src = format!("{PRELUDE}{scenario}");
    let _code = py.run(&src);
    let stacks = py.drain();
    assert_eq!(errors(), [0; 4]);
    assert_eq!(stacks.len(), 1, "{stacks:?}");
    let stack = &stacks[0];
    let mut expected_names = vec!["trigger"];
    let marked = src
        .lines()
        .filter_map(|l| l.split_once("#> ").map(|(_, n)| n));
    expected_names.extend(marked.filter(|&n| n != "trigger"));
    let expected = expect(&src, &expected_names);
    if scenario == THREAD {
        assert_eq!(stack[..2], expected[..], "{stack:#?}");
        assert_eq!(
            names(&stack[2..]),
            ["Thread.run", "Thread._bootstrap_inner", "Thread._bootstrap"]
        );
    } else {
        assert_eq!(*stack, expected, "{stack:#?}");
    }
}

fn check_spin(py: &Py, mode: Mode) {
    let timer = Timer::start(mode, Duration::from_millis(1)).unwrap();
    #[cfg(target_os = "linux")]
    if mode == Mode::CTimer {
        timer.thread_created(signal_timer::gettid()).unwrap();
    }
    let _code = py.run(SPIN);
    drop(timer);
    let stacks = py.drain();
    assert_eq!(errors(), [0; 4]);
    assert!(stacks.len() >= 50, "{} samples", stacks.len());
    for stack in &stacks {
        let names = names(stack);
        let full = ["spin", "outer", "<module>"];
        assert!(full.ends_with(&names) && !names.is_empty(), "{names:?}");
    }
    let spins = stacks.iter().filter(|s| s[0].name == "spin").count();
    assert!(
        spins * 2 > stacks.len(),
        "{spins} of {} in spin",
        stacks.len()
    );
}

#[test]
fn cpython() {
    Python::attach(|py| {
        let version = py.version_info();
        assert_eq!(
            (version.major, version.minor),
            (3, 14),
            "set PYO3_PYTHON=python3.14"
        );
        assert!(CODE_TYPE.set(&raw const ffi::PyCode_Type as usize).is_ok());
        let py = Py {
            py,
            globals: PyDict::new(py),
        };
        py.globals.set_item("__name__", "__main__").unwrap();
        let _handler = SignalHandler::install(Timer::SIGNAL, on_signal).unwrap();

        for checked in [true, false] {
            CHECKED.store(checked, Ordering::Relaxed);
            for scenario in [CALLS, ENTRY_FRAMES, THREAD] {
                check_triggered(&py, scenario);
            }

            let _code = py.run(&format!("{PRELUDE}{DEEP}"));
            assert!(py.drain().is_empty());
            assert_eq!(errors(), [0, 0, 1, 0]);

            check_spin(&py, Mode::ITimer);
            #[cfg(target_os = "linux")]
            check_spin(&py, Mode::CTimer);
        }
        assert_eq!(BUSY.load(Ordering::Relaxed), 0);
    });
}
