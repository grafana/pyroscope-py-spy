use std::sync::Arc;

use anyhow::{Context, Error, Result};

use remoteprocess::{Pid, ProcessMemory};
use serde_derive::Serialize;

use crate::config::{Config, LineNo};
use crate::python_data_access::{copy_bytes, copy_string};
use crate::python_interpreters::{
    CodeObject, FrameObject, InterpreterState, ThreadState, TupleObject,
};

/// Call stack for a single python thread
#[derive(Debug, Clone, Serialize)]
pub struct StackTrace {
    /// The process id than generated this stack trace
    pub pid: Pid,
    /// The python thread id for this stack trace
    pub thread_id: u64,
    // The python thread name for this stack trace
    pub thread_name: Option<String>,
    /// The OS thread id for this stack tracee
    pub os_thread_id: Option<u64>,
    /// Whether or not the thread was active
    pub active: bool,
    /// Whether or not the thread held the GIL
    pub owns_gil: bool,
    /// Whether Python unwinding failed; the frames contain only a synthetic <error> frame.
    pub error: bool,
    /// The frames
    pub frames: Vec<Frame>,
    /// process commandline / parent process info
    pub process_info: Option<Arc<ProcessInfo>>,
}

/// Information about a single function call in a stack trace
#[derive(Debug, Hash, Eq, PartialEq, Ord, PartialOrd, Clone, Serialize)]
pub struct Frame {
    /// The function name
    pub name: String,
    /// The full filename of the file
    pub filename: String,
    /// The module/shared library the
    pub module: Option<String>,
    /// A short, more readable, representation of the filename
    pub short_filename: Option<String>,
    /// The line number inside the file (or 0 for native frames without line information)
    pub line: i32,
    /// Local Variables associated with the frame
    pub locals: Option<Vec<LocalVariable>>,
    /// If this is an entry frame. Each entry frame corresponds to one native frame (Python 3.11)
    pub is_entry: bool,
    /// If the last frame was a shim. This is used in Python 3.12+ to detect entry frames.
    pub is_shim_entry: bool,
}

#[derive(Debug, Hash, Eq, PartialEq, Ord, PartialOrd, Clone, Serialize)]
pub struct LocalVariable {
    pub name: String,
    pub addr: usize,
    pub arg: bool,
    pub repr: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProcessInfo {
    pub pid: Pid,
    pub command_line: String,
    pub parent: Option<Box<ProcessInfo>>,
}

/// Given an InterpreterState, this function returns a vector of stack traces for each thread
pub fn get_stack_traces<I, P>(
    interpreter_address: usize,
    process: &P,
    threadstate_address: usize,
    config: Option<&Config>,
) -> Result<Vec<StackTrace>, Error>
where
    I: InterpreterState,
    P: ProcessMemory,
{
    let gil_thread_id = get_gil_threadid::<I, P>(threadstate_address, process)?;

    let threadstate_ptr_ptr = I::threadstate_ptr_ptr(interpreter_address);
    let mut threads: *const I::ThreadState = process
        .copy_struct(threadstate_ptr_ptr as usize)
        .context("Failed to copy PyThreadState head pointer")?;

    let mut ret = Vec::new();

    let lineno = config.map(|c| c.lineno).unwrap_or(LineNo::NoLine);
    let dump_locals = config.map(|c| c.dump_locals).unwrap_or(0);

    while !threads.is_null() {
        let thread = process
            .copy_pointer(threads)
            .context("Failed to copy PyThreadState")?;

        let mut trace = get_stack_trace(&thread, process, dump_locals > 0, lineno)?;
        trace.owns_gil = trace.thread_id == gil_thread_id;

        ret.push(trace);
        // This seems to happen occasionally when scanning BSS addresses for valid interpreters
        if ret.len() > 4096 {
            return Err(format_err!("Max thread recursion depth reached"));
        }
        threads = thread.next();
    }
    Ok(ret)
}

/// Samples a thread without letting an unwind failure discard the other threads.
/// Interpreter discovery uses the fallible `get_stack_trace` instead.
pub(crate) fn get_stack_trace_or_error<T: ThreadState, P: ProcessMemory>(
    thread: &T,
    process: &P,
    copy_locals: bool,
    lineno: LineNo,
) -> StackTrace {
    get_stack_trace(thread, process, copy_locals, lineno).unwrap_or_else(|error| {
        debug!(
            "Failed to unwind thread {}: {:#}",
            thread.thread_id(),
            error
        );
        StackTrace {
            pid: 0,
            thread_id: thread.thread_id(),
            thread_name: None,
            os_thread_id: thread.native_thread_id(),
            active: true,
            owns_gil: false,
            error: true,
            frames: vec![Frame {
                name: "<error>".to_owned(),
                filename: String::new(),
                module: None,
                short_filename: None,
                line: 0,
                locals: None,
                is_entry: false,
                is_shim_entry: false,
            }],
            process_info: None,
        }
    })
}

/// Gets a stack trace for an individual thread
pub fn get_stack_trace<T, P>(
    thread: &T,
    process: &P,
    copy_locals: bool,
    lineno: LineNo,
) -> Result<StackTrace, Error>
where
    T: ThreadState,
    P: ProcessMemory,
{
    // TODO: just return frames here? everything else probably should be returned out of scope
    let mut frames = Vec::new();

    // python 3.11+ has an extra level of indirection to get the Frame from the threadstate
    let mut frame_address = thread.frame_address();
    if let Some(addr) = frame_address {
        frame_address = Some(process.copy_struct(addr)?);
    }

    let mut frame_ptr = thread.frame(frame_address);

    // We are iterating in reverse, i.e. from last call to first call.
    // Since Python 3.12, there are shim frames inserted before a block
    // of Python frames. When we encounter one, update the last frame.
    let set_last_frame_as_shim_entry = &mut |frames: &mut Vec<Frame>| {
        if let Some(frame) = frames.last_mut() {
            frame.is_shim_entry = true;
        }
    };

    // Count skipped shims too, so a corrupt chain cannot loop forever.
    let mut frame_count = 0;
    while !frame_ptr.is_null() {
        frame_count += 1;
        if frame_count > 4096 {
            return Err(format_err!("Max frame recursion depth reached"));
        }
        let frame = process
            .copy_pointer(frame_ptr)
            .context("Failed to copy PyFrameObject")?;

        // C-stack shim frames may not contain a code object (Python 3.13+).
        // Identify them before reading code or strings, so real read errors propagate.
        if frame.is_shim() {
            frame_ptr = frame.back();
            set_last_frame_as_shim_entry(&mut frames);
            continue;
        }

        let code = process
            .copy_pointer(frame.code())
            .context("Failed to copy PyCodeObject")?;

        let filename = copy_string(code.filename(), process).context("Failed to copy filename")?;

        // Try to get qualname first (available in Python 3.11+), fall back to name
        let name = match code.qualname() {
            Some(qualname_ptr) => {
                copy_string(qualname_ptr, process).or_else(|_| copy_string(code.name(), process))
            }
            None => copy_string(code.name(), process),
        }
        .context("Failed to copy function name")?;

        // skip <shim> entries in python 3.12+
        // Unset file/function name in py3.13 means this is a shim.
        if filename.is_empty() || filename == "<shim>" {
            frame_ptr = frame.back();
            set_last_frame_as_shim_entry(&mut frames);
            continue;
        }

        let line = match lineno {
            LineNo::NoLine => 0,
            LineNo::First => code.first_lineno(),
            LineNo::LastInstruction => match get_line_number(&code, frame.lasti(), process) {
                Ok(line) => line,
                Err(e) => {
                    // Failling to get the line number really shouldn't be fatal here, but
                    // can happen in extreme cases (https://github.com/benfred/py-spy/issues/164)
                    // Rather than fail set the linenumber to 0. This is used by the native extensions
                    // to indicate that we can't load a line number and it should be handled gracefully
                    warn!(
                        "Failed to get line number from {}.{}: {}",
                        filename, name, e
                    );
                    0
                }
            },
        };

        let locals = if copy_locals {
            Some(
                get_locals(&code, frame_ptr, &frame, process)
                    .context("Failed to get local variables")?,
            )
        } else {
            None
        };

        let is_entry = frame.is_entry();

        frames.push(Frame {
            name,
            filename,
            line,
            short_filename: None,
            module: None,
            locals,
            is_entry,
            is_shim_entry: false,
        });
        frame_ptr = frame.back();
    }

    // First frame is always a shim
    set_last_frame_as_shim_entry(&mut frames);

    Ok(StackTrace {
        pid: 0,
        frames,
        thread_id: thread.thread_id(),
        thread_name: None,
        owns_gil: false,
        error: false,
        active: true,
        os_thread_id: thread.native_thread_id(),
        process_info: None,
    })
}

impl StackTrace {
    pub fn status_str(&self) -> &str {
        match (self.owns_gil, self.active) {
            (_, false) => "idle",
            (true, true) => "active+gil",
            (false, true) => "active",
        }
    }

    pub fn format_threadid(&self) -> String {
        // native threadids in osx are kinda useless, use the pthread id instead
        #[cfg(target_os = "macos")]
        return format!("{:#X}", self.thread_id);

        // otherwise use the native threadid if given
        #[cfg(not(target_os = "macos"))]
        match self.os_thread_id {
            Some(tid) => format!("{}", tid),
            None => format!("{:#X}", self.thread_id),
        }
    }
}

/// Returns the line number from a PyCodeObject (given the lasti index from a PyFrameObject)
fn get_line_number<C: CodeObject, P: ProcessMemory>(
    code: &C,
    lasti: i32,
    process: &P,
) -> Result<i32, Error> {
    let table =
        copy_bytes(code.line_table(), process).context("Failed to copy line number table")?;
    Ok(code.get_line_number(lasti, &table))
}

fn get_locals<C: CodeObject, F: FrameObject, P: ProcessMemory>(
    code: &C,
    frameptr: *const F,
    frame: &F,
    process: &P,
) -> Result<Vec<LocalVariable>, Error> {
    let local_count = code.nlocals() as usize;
    let argcount = code.argcount() as usize;
    let varnames = process
        .copy_pointer(code.varnames())
        .context("Failed to get varnames from PyCodeObject")?;

    let ptr_size = std::mem::size_of::<*const i32>();
    let locals_addr = frameptr as usize + std::mem::size_of_val(frame) - ptr_size;

    let mut ret = Vec::new();

    for i in 0..local_count {
        let nameptr: *const C::StringObject =
            process.copy_struct(varnames.address(code.varnames() as usize, i))?;

        let name = copy_string(nameptr, process).context("Failed to copy local variable name")?;
        let addr: usize = process.copy_struct(locals_addr + i * ptr_size)?;

        // hack: handle things like None, True, False, small integer constants etc on Python 3.14
        let addr = if addr & 1 == 1 { addr - 1 } else { addr };

        if addr == 0 {
            continue;
        }
        ret.push(LocalVariable {
            name,
            addr,
            arg: i < argcount,
            repr: None,
        });
    }
    Ok(ret)
}

pub fn get_gil_threadid<I: InterpreterState, P: ProcessMemory>(
    threadstate_address: usize,
    process: &P,
) -> Result<u64, Error> {
    // happens during initialization when checking to see if we have a valid interpreter (before we've figured out the threadstate_address)
    if threadstate_address == 0 {
        return Ok(0);
    }

    let addr = if I::HAS_GIL_RUNTIME_STATE {
        // get the gilruntimestate - note that this struct is identical between 3.12/3.13/3.14
        let gil_state: crate::python_bindings::v3_13_0::_gil_runtime_state =
            process.copy_struct(threadstate_address)?;
        // check to see if the GIL is locked already
        if gil_state.locked != 0 {
            gil_state.last_holder as usize
        } else {
            0
        }
    } else {
        process.copy_struct::<usize>(threadstate_address)?
    };

    // if the addr is 0, no thread is currently holding the GIL
    let threadid = if addr != 0 {
        let threadstate: I::ThreadState = process.copy_struct(addr)?;
        threadstate.thread_id()
    } else {
        0
    };

    Ok(threadid)
}

impl ProcessInfo {
    pub fn to_frame(&self) -> Frame {
        Frame {
            name: format!("process {}:\"{}\"", self.pid, self.command_line),
            filename: String::from(""),
            module: None,
            short_filename: None,
            line: 0,
            locals: None,
            is_entry: true,
            is_shim_entry: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::python_bindings::v3_7_0::PyCodeObject;
    use crate::python_bindings::{v3_11_0 as py, v3_12_0, v3_13_0, v3_14_0};
    use crate::python_data_access::tests::{to_asciiobject, to_byteobject, AllocatedPyASCIIObject};
    use remoteprocess::LocalProcess;

    // All pointers refer to live, boxed test objects. Fail selected reads without
    // dereferencing invalid pointers in LocalProcess.
    struct TestMemory {
        fail_at: usize,
    }

    impl ProcessMemory for TestMemory {
        fn read(&self, addr: usize, buf: &mut [u8]) -> Result<(), remoteprocess::Error> {
            if addr == 0 || addr == self.fail_at {
                return Err(remoteprocess::Error::Other("injected read failure".into()));
            }
            LocalProcess.read(addr, buf)
        }
    }

    struct TestStack {
        filename: Box<AllocatedPyASCIIObject>,
        _name: Box<AllocatedPyASCIIObject>,
        code: Box<py::PyCodeObject>,
        frame: Box<py::_PyInterpreterFrame>,
        cframe: Box<py::_PyCFrame>,
        thread: py::PyThreadState,
    }

    impl TestStack {
        fn new() -> Self {
            let mut filename = Box::new(to_asciiobject("test.py"));
            let mut name = Box::new(to_asciiobject("function"));
            let mut code = Box::new(py::PyCodeObject {
                co_filename: std::ptr::from_mut(&mut filename.base).cast(),
                co_name: std::ptr::from_mut(&mut name.base).cast(),
                co_qualname: std::ptr::from_mut(&mut name.base).cast(),
                co_firstlineno: 42,
                ..Default::default()
            });
            let mut frame = Box::new(py::_PyInterpreterFrame {
                f_code: &mut *code,
                // Python 3.11 entry frames must not be mistaken for shims.
                is_entry: 1,
                ..Default::default()
            });
            frame.prev_instr = code.co_code_adaptive.as_mut_ptr().cast();
            let mut cframe = Box::new(py::_PyCFrame {
                current_frame: &mut *frame,
                ..Default::default()
            });
            let thread = py::PyThreadState {
                thread_id: 123,
                native_thread_id: 456,
                cframe: &mut *cframe,
                ..Default::default()
            };
            Self {
                filename,
                _name: name,
                code,
                frame,
                cframe,
                thread,
            }
        }

        fn sample(&self, fail_at: usize) -> StackTrace {
            get_stack_trace_or_error(&self.thread, &TestMemory { fail_at }, false, LineNo::First)
        }
    }

    fn assert_error_trace(trace: &StackTrace) {
        assert!(trace.error);
        assert_eq!(trace.thread_id, 123);
        assert_eq!(trace.os_thread_id, Some(456));
        assert_eq!(trace.frames.len(), 1);
        assert_eq!(
            trace.frames[0],
            Frame {
                name: "<error>".into(),
                filename: String::new(),
                line: 0,
                module: None,
                short_filename: None,
                locals: None,
                is_entry: false,
                is_shim_entry: false,
            }
        );
    }

    #[test]
    fn test_unwind_read_errors() {
        let stack = TestStack::new();
        for addr in [
            stack.thread.frame_address().unwrap(),
            &*stack.frame as *const _ as usize,
            &*stack.code as *const _ as usize,
            stack.code.co_filename as usize,
            stack.code.co_name as usize,
        ] {
            assert_error_trace(&stack.sample(addr));
            // Discovery must still reject an unreadable candidate interpreter.
            assert!(get_stack_trace(
                &stack.thread,
                &TestMemory { fail_at: addr },
                false,
                LineNo::First
            )
            .is_err());
        }
    }

    #[test]
    fn test_partial_stack_is_discarded() {
        let mut stack = TestStack::new();
        let mut caller = py::_PyInterpreterFrame::default();
        stack.frame.previous = &mut caller;
        assert_error_trace(&stack.sample(&caller as *const _ as usize));
    }

    #[test]
    fn test_invalid_ucs4_strings() {
        let mut stack = TestStack::new();
        let mut invalid = 0x110000u32;
        let mut string = py::PyUnicodeObject::default();
        string._base._base.length = 1;
        string._base._base.state.set_kind(4);
        string.data.ucs4 = &mut invalid;
        let ptr = (&mut string as *mut py::PyUnicodeObject).cast();
        let filename = stack.code.co_filename;
        stack.code.co_filename = ptr;
        assert_error_trace(&stack.sample(0));
        stack.code.co_filename = filename;
        stack.code.co_qualname = ptr;
        let fallback = stack.sample(0);
        assert!(!fallback.error);
        assert_eq!(fallback.frames[0].name, "function");
        stack.code.co_name = ptr;
        assert_error_trace(&stack.sample(0));
    }

    #[test]
    fn test_success_empty_and_qualname_fallback() {
        let mut stack = TestStack::new();
        let trace = stack.sample(0);
        assert!(!trace.error);
        assert_eq!(trace.frames.len(), 1);
        assert_eq!(trace.frames[0].name, "function");
        assert_eq!(trace.frames[0].line, 42);
        assert!(trace.frames[0].is_entry);

        stack.code.co_qualname = std::ptr::null_mut();
        let trace = stack.sample(0);
        assert!(!trace.error);
        assert_eq!(trace.frames[0].name, "function");

        stack.cframe.current_frame = std::ptr::null_mut();
        let trace = stack.sample(0);
        assert!(!trace.error);
        assert!(trace.frames.is_empty());
    }

    #[test]
    fn test_line_number_failure_is_not_an_error_trace() {
        let stack = TestStack::new();
        let trace = get_stack_trace_or_error(
            &stack.thread,
            &TestMemory { fail_at: 0 },
            false,
            LineNo::LastInstruction,
        );
        assert!(!trace.error);
        assert_eq!(trace.frames[0].line, 0);
    }

    #[test]
    fn test_python_spy_keeps_healthy_threads_and_error_metadata() {
        use crate::{config::LockingStrategy, python_spy::PythonSpy, version::Version};

        let mut broken = TestStack::new();
        let mut healthy = TestStack::new();
        // A malformed string exercises the complete sampling path without races
        // or needing to attach to another process.
        broken.filename.base.length = 4096;
        healthy.thread.thread_id = 789;
        healthy.thread.native_thread_id = 987;
        broken.thread.next = &mut healthy.thread;
        let gil_holder = &mut broken.thread as *mut _;
        let mut interpreter = py::PyInterpreterState::default();
        interpreter.threads.head = &mut broken.thread;
        let pid = std::process::id() as Pid;
        let mut spy = PythonSpy {
            pid,
            process: remoteprocess::Process::new(pid).unwrap(),
            version: Version {
                major: 3,
                minor: 11,
                patch: 0,
                release_flags: String::new(),
                build_metadata: None,
            },
            interpreter_address: &interpreter as *const _ as usize,
            threadstate_address: &gil_holder as *const _ as usize,
            config: Config {
                blocking: LockingStrategy::AlreadyLocked,
                lineno: LineNo::First,
                ..Default::default()
            },
            #[cfg(feature = "unwind")]
            native: None,
            short_filenames: Default::default(),
            python_thread_ids: Default::default(),
            python_thread_names: [(123, "broken".to_owned()), (789, "healthy".to_owned())].into(),
            debug_offsets: None,
            #[cfg(target_os = "linux")]
            dockerized: false,
        };
        let traces = spy.get_stack_traces().unwrap();
        assert_eq!(traces.len(), 2);
        let broken = &traces[0];
        assert!(broken.error);
        assert_eq!(broken.pid, pid);
        assert_eq!(broken.thread_id, 123);
        assert_eq!(broken.os_thread_id, Some(456));
        assert_eq!(broken.thread_name.as_deref(), Some("broken"));
        assert!(broken.owns_gil);
        assert_eq!(broken.frames.len(), 1);
        assert_eq!(broken.frames[0].name, "<error>");
        let healthy = &traces[1];
        assert!(!healthy.error);
        assert_eq!(healthy.pid, pid);
        assert_eq!(healthy.thread_id, 789);
        assert_eq!(healthy.thread_name.as_deref(), Some("healthy"));
        assert!(!healthy.owns_gil);
        assert_eq!(healthy.frames[0].name, "function");

        spy.config.gil_only = true;
        let traces = spy.get_stack_traces().unwrap();
        assert_eq!(traces.len(), 1);
        assert!(traces[0].error);
        assert!(traces[0].owns_gil);
    }

    #[test]
    fn test_shims_skip_code_reads() {
        macro_rules! check_shim {
            ($py:ident) => {{
                let mut shim = $py::_PyInterpreterFrame {
                    owner: 3,
                    ..Default::default()
                };
                let trace = get_stack_trace_or_error(
                    &FrameThread::<$py::PyThreadState> { frame: &mut shim },
                    &TestMemory { fail_at: 0 },
                    false,
                    LineNo::NoLine,
                );
                assert!(!trace.error);
                assert!(trace.frames.is_empty());
                // A cycle consisting only of shims must still hit the depth limit.
                shim.previous = &mut shim;
                let trace = get_stack_trace_or_error(
                    &FrameThread::<$py::PyThreadState> { frame: &mut shim },
                    &TestMemory { fail_at: 0 },
                    false,
                    LineNo::NoLine,
                );
                assert_error_trace(&trace);
            }};
        }
        check_shim!(v3_12_0);
        check_shim!(v3_13_0);
        check_shim!(v3_14_0);
    }

    // Expose a frame chain directly while retaining each Python version's real
    // FrameObject implementation, without coupling tests to thread indirection.
    #[derive(Clone, Copy)]
    struct FrameThread<T: ThreadState> {
        frame: *mut T::FrameObject,
    }

    impl<T: ThreadState> ThreadState for FrameThread<T> {
        type FrameObject = T::FrameObject;
        type InterpreterState = T::InterpreterState;
        fn interp(&self) -> *mut Self::InterpreterState {
            std::ptr::null_mut()
        }
        fn frame_address(&self) -> Option<usize> {
            None
        }
        fn frame(&self, _: Option<usize>) -> *mut Self::FrameObject {
            self.frame
        }
        fn thread_id(&self) -> u64 {
            123
        }
        fn native_thread_id(&self) -> Option<u64> {
            Some(456)
        }
        fn next(&self) -> *mut Self {
            std::ptr::null_mut()
        }
    }

    #[test]
    fn test_get_line_number() {
        let mut lnotab = to_byteobject(&[0u8, 1, 10, 1, 8, 1, 4, 1]);
        let code = PyCodeObject {
            co_firstlineno: 3,
            co_lnotab: &mut lnotab.base.ob_base.ob_base,
            ..Default::default()
        };
        let lineno = get_line_number(&code, 30, &LocalProcess).unwrap();
        assert_eq!(lineno, 7);
    }
}
