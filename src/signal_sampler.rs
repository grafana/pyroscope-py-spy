//! Samples the current process by unwinding python stacks inside a `SIGPROF` handler.
//!
//! The handler lives in [`unwind::handler`]. It records code object addresses into a
//! ring buffer. A normal thread drains it and resolves names, tolerating code objects that were
//! freed in the meantime. A tick interrupts one running thread, so only threads on the CPU are
//! sampled.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Error};
use remoteprocess::{Pid, Process};

use crate::config::{Config, LineNo, SignalTimer};
use crate::python_bindings::{v3_11_0, v3_12_0, v3_13_0, v3_14_0};
use crate::python_interpreters::FrameObject;
use crate::python_process_info::{PythonProcessLayout, TypeChecks};
use crate::stack_trace::{resolve_frames, RawFrame, StackTrace};
use crate::version::Version;
use ringbuffer::PopError;
use timer::{Mode, SignalHandler, Timer};
use unwind::handler;
use unwind::{Frame, ThreadStateGetter, V3_11, V3_12, V3_13, V3_14};

#[doc(hidden)]
pub mod ringbuffer;
#[doc(hidden)]
pub mod timer;
#[doc(hidden)]
pub mod unwind;

const MAX_FRAMES: usize = 4096; //todo wtf

pub struct PythonSpySend { // subset of PySpy so that we can send it
    pub pid: Pid,
    pub process: Process,
    pub version: Version,
    pub addresses: PythonProcessLayout,
    pub config: Config,
}

fn lookup(name: &std::ffi::CStr) -> Result<usize, Error> {
    let addr = unsafe { libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr()) } as usize;
    if addr == 0 {
        return Err(format_err!("symbol {:?} not found", name));
    }
    Ok(addr)
}

fn lookup_thread_state_getter() -> Result<ThreadStateGetter, Error> {
    // TODO: PyGILState_GetThisThreadState is not safe on 3.15; find another way to get the tstate.
    let addr = lookup(c"PyGILState_GetThisThreadState")?;
    // SAFETY: the symbol has this signature.
    Ok(unsafe { std::mem::transmute::<usize, ThreadStateGetter>(addr) })
}

type Configure = fn(usize, ThreadStateGetter, usize);

fn configure_for(version: &Version) -> Result<Configure, Error> {
    Ok(match (version.major, version.minor) {
        (3, 11) => handler::configure::<V3_11>,
        (3, 12) => handler::configure::<V3_12>,
        (3, 13) => handler::configure::<V3_13>,
        (3, 14) => handler::configure::<V3_14>,
        _ => {
            return Err(format_err!(
                "unwinding in the signal handler supports only Python 3.11 to 3.14, found {}",
                version
            ))
        }
    })
}

/// The timer of a running [`SignalHandlerSession`], shared with the callers of
/// [`thread_created`] and [`thread_removed`]. `None` once the session ended.
pub type SharedTimer = Arc<Mutex<Option<Timer>>>;

/// Running signal sampling. Only one can exist at a time.
pub struct SignalHandlerSession {
    timer: SharedTimer,
    // Dropped after the timer, see the `Drop` impl.
    _handler: SignalHandler,
    buf: Vec<Frame>,
}

impl SignalHandlerSession {
    pub fn start(spy: &PythonSpySend) -> Result<SignalHandlerSession, Error> {
        let configure = configure_for(&spy.version)?;
        if spy.pid != std::process::id() as Pid {
            return Err(format_err!(
                "unwinding in the signal handler works only for the current process"
            ));
        }
        unwind::init()
            .map_err(|e| format_err!("failed to install memory fault handlers: {:?}", e))?;
        let TypeChecks::Strict(types) = spy.addresses.type_checks else {
            return Err(format_err!(
                "unwinding in the signal handler requires strict type checks"
            ));
        };
        configure(types.code, lookup_thread_state_getter()?, MAX_FRAMES);

        // Discard what a previous session left behind.
        let mut buf = vec![Frame::default(); MAX_FRAMES];
        if let Some(mut consumer) = handler::consumer() {
            while consumer.pop_slice(&mut buf).is_ok() {}
        }

        let signal_handler = SignalHandler::install(Timer::SIGNAL, handler::handler)?;
        let interval = Duration::from_secs_f64(1.0 / spy.config.sampling_rate as f64);
        let mode = match spy.config.signal_timer {
            SignalTimer::ITimer => Mode::ITimer,
            #[cfg(target_os = "linux")]
            SignalTimer::CTimer => Mode::CTimer,
            #[cfg(not(target_os = "linux"))]
            SignalTimer::CTimer => return Err(format_err!("CTimer works only on Linux")),
        };
        let timer = Timer::start(mode, interval)?;
        Ok(SignalHandlerSession {
            timer: Arc::new(Mutex::new(Some(timer))),
            _handler: signal_handler,
            buf,
        })
    }

    pub fn timer(&self) -> SharedTimer {
        self.timer.clone()
    }

    /// Resolves everything the handler recorded since the last call.
    pub fn drain(&mut self, spy: &PythonSpySend) -> (Vec<StackTrace>, Vec<(Pid, Error)>) {
        let mut traces = Vec::new();
        let mut errors = Vec::new();
        let Some(mut consumer) = handler::consumer() else {
            return (traces, errors);
        };
        loop {
            let n = match consumer.pop_slice(&mut self.buf) {
                Ok(n) => n,
                Err(PopError::Empty) => break,
                Err(PopError::TooSmall { .. }) => unreachable!("buffer fits MAX_FRAMES"),
            };
            match resolve_signal_stack(spy, &self.buf[..n]) {
                Ok(trace) => traces.push(trace),
                Err(e) => errors.push((spy.pid, e)),
            }
        }
        (traces, errors)
    }
}

/// Builds the stack trace of a thread from frames recorded by a signal handler. Whether the
/// thread holds the GIL is unknown, so `owns_gil` is false and `gil_only` does not apply.
/// Line numbers and thread ids are not recorded, so every frame has line 0 and the thread id
/// is 0 with no thread name.
/// todo make this method generic, avoid version dispatch
fn resolve_signal_stack(spy: &PythonSpySend, frames: &[Frame]) -> Result<StackTrace, Error> {
    let (process, checks) = (&spy.process, &spy.addresses.type_checks);
    let frames = match (spy.version.major, spy.version.minor) {
        (3, 11) => resolve_signal_frames::<v3_11_0::_PyInterpreterFrame>(process, frames, checks),
        (3, 12) => resolve_signal_frames::<v3_12_0::_PyInterpreterFrame>(process, frames, checks),
        (3, 13) => resolve_signal_frames::<v3_13_0::_PyInterpreterFrame>(process, frames, checks),
        (3, 14) => resolve_signal_frames::<v3_14_0::_PyInterpreterFrame>(process, frames, checks),
        _ => Err(format_err!(
            "Unsupported version of Python: {}",
            spy.version
        )),
    }
    .context("Failed to resolve frames")?;
    Ok(StackTrace {
        pid: 0,
        thread_id: 0,
        thread_name: None,
        os_thread_id: None,
        active: true,
        owns_gil: false,
        frames,
        process_info: None,
    })
}

fn resolve_signal_frames<F: FrameObject>(
    process: &Process,
    frames: &[Frame],
    type_checks: &TypeChecks,
) -> Result<Vec<crate::stack_trace::Frame>, Error> {
    let raw: Vec<RawFrame> = frames
        .iter()
        .map(|f| RawFrame {
            frame: 0,
            code: f.code,
            lasti: 0,
            is_entry: false,
            is_shim_entry: false,
        })
        .collect();
    resolve_frames::<F, Process>(&raw, process, false, LineNo::NoLine, type_checks)
}

impl Drop for SignalHandlerSession {
    /// Stops the timers before the handler is removed. Holding the lock keeps a concurrent hook
    /// from using a dead timer.
    fn drop(&mut self) {
        *self.timer.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

/// Starts sampling `tid` if the session runs in [`Mode::CTimer`]. Does nothing otherwise.
pub fn thread_created(timer: &SharedTimer, tid: timer::pid_t) -> Result<(), Error> {
    match timer.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
        Some(timer) => Ok(timer.thread_created(tid)?),
        None => Ok(()),
    }
}

/// Stops sampling `tid`. Does nothing if the session ended or runs in [`Mode::ITimer`].
pub fn thread_removed(timer: &SharedTimer, tid: timer::pid_t) {
    if let Some(timer) = timer.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
        timer.thread_removed(tid);
    }
}
