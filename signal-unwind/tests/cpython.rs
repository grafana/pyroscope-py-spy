//! Unwinds the CPython that pyo3 links, 3.11 to 3.14: build with e.g. `PYO3_PYTHON=python3.14`.

use pyo3::ffi;
use pyo3::prelude::*;
use pyo3::types::PyDict;
use signal_timer::{Mode, SignalHandler, Timer};
use signal_unwind::handler;
use signal_unwind::{Frame, V3_11, V3_12, V3_13, V3_14};
use std::time::Duration;

const MAX_FRAMES: usize = 128;

// TODO: PyGILState_GetThisThreadState is not safe on 3.15; find another way to get the tstate.
extern "C" fn this_thread_state() -> usize {
    unsafe { ffi::PyGILState_GetThisThreadState() as usize }
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
    fn drain(&self) -> Vec<Vec<String>> {
        let mut c = handler::consumer().unwrap();
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
                    Some(attr("co_qualname"))
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

fn expect(names: &[&str]) -> Vec<String> {
    names.iter().map(|name| name.to_string()).collect()
}

fn names(stack: &[String]) -> Vec<&str> {
    stack.iter().map(String::as_str).collect()
}

fn check_triggered(py: &Py, scenario: &str) {
    let src = format!("{PRELUDE}{scenario}");
    let _code = py.run(&src);
    let stacks = py.drain();
    assert_eq!(handler::take_dropped(), 0);
    assert_eq!(stacks.len(), 1, "{stacks:?}");
    let stack = &stacks[0];
    let mut expected_names = vec!["trigger"];
    let marked = src
        .lines()
        .filter_map(|l| l.split_once("#> ").map(|(_, n)| n));
    expected_names.extend(marked.filter(|&n| n != "trigger"));
    let expected = expect(&expected_names);
    if scenario == THREAD {
        assert_eq!(stack[..2], expected[..], "{stack:#?}");
        let rest = ["Thread.run", "Thread._bootstrap_inner", "Thread._bootstrap"];
        assert_eq!(stack[2..], expect(&rest)[..], "{stack:#?}");
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
    assert_eq!(handler::take_dropped(), 0);
    assert!(stacks.len() >= 50, "{} samples", stacks.len());
    for stack in &stacks {
        let names = names(stack);
        let full = ["spin", "outer", "<module>"];
        assert!(full.ends_with(&names) && !names.is_empty(), "{names:?}");
    }
    let spins = stacks.iter().filter(|s| s[0] == "spin").count();
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
        assert_eq!(version.major, 3);
        assert!(
            (11..=14).contains(&version.minor),
            "unsupported {version:?}"
        );
        let minor = version.minor;
        let py = Py {
            py,
            globals: PyDict::new(py),
        };
        py.globals.set_item("__name__", "__main__").unwrap();
        signal_unwind::init().unwrap();
        handler::configure(
            &raw const ffi::PyCode_Type as usize,
            this_thread_state,
            MAX_FRAMES,
        );
        let handler = match minor {
            11 => handler::handler::<V3_11>(),
            12 => handler::handler::<V3_12>(),
            13 => handler::handler::<V3_13>(),
            _ => handler::handler::<V3_14>(),
        };
        let _handler = SignalHandler::install(Timer::SIGNAL, handler).unwrap();

        {
            for scenario in [CALLS, ENTRY_FRAMES, THREAD] {
                check_triggered(&py, scenario);
            }

            let _code = py.run(&format!("{PRELUDE}{DEEP}"));
            assert!(py.drain().is_empty());
            assert_eq!(handler::take_dropped(), 1);

            check_spin(&py, Mode::ITimer);
            #[cfg(target_os = "linux")]
            check_spin(&py, Mode::CTimer);
        }
        assert_eq!(handler::take_dropped(), 0);
    });
}
