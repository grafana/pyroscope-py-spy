//! Samples the current process by unwinding python stacks inside a `SIGPROF` handler.
//!
//! The handlers live in [`signal_unwind::handler`]. They record code object addresses into a
//! ring buffer. A normal thread drains it and resolves names, tolerating code objects that were
//! freed in the meantime. A tick interrupts one running thread, so only threads on the CPU are
//! sampled.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Error;
use remoteprocess::Pid;
use signal_ringbuffer::PopError;
use signal_timer::{Mode, SignalHandler, Timer};
use signal_unwind::handler::{self, Handler, ThreadStateGetter};
use signal_unwind::{Frame, V3_11, V3_12, V3_13, V3_14};

use crate::config::SignalTimer;
use crate::python_spy::PythonSpy;
use crate::stack_trace::StackTrace;

const MAX_FRAMES: usize = 4096;

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

fn handler_for(minor: u64) -> Option<Handler> {
    Some(match minor {
        11 => handler::handler::<V3_11>(),
        12 => handler::handler::<V3_12>(),
        13 => handler::handler::<V3_13>(),
        14 => handler::handler::<V3_14>(),
        _ => return None,
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
    pub fn start(spy: &PythonSpy) -> Result<SignalHandlerSession, Error> {
        let on_signal = match spy.version.major {
            3 => handler_for(spy.version.minor),
            _ => None,
        }
        .ok_or_else(|| {
            format_err!(
                "unwinding in the signal handler supports only Python 3.11 to 3.14, found {}",
                spy.version
            )
        })?;
        if spy.pid != std::process::id() as Pid {
            return Err(format_err!(
                "unwinding in the signal handler works only for the current process"
            ));
        }
        signal_unwind::init()
            .map_err(|e| format_err!("failed to install memory fault handlers: {:?}", e))?;
        let code_type = lookup(c"PyCode_Type")?;
        handler::configure(code_type, lookup_thread_state_getter()?, MAX_FRAMES);

        // Discard what a previous session left behind.
        let mut buf = vec![Frame::default(); MAX_FRAMES];
        if let Some(mut consumer) = handler::consumer() {
            while consumer.pop_slice(&mut buf).is_ok() {}
        }
        handler::take_dropped();

        let signal_handler = SignalHandler::install(Timer::SIGNAL, on_signal)?;
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
    pub fn drain(&mut self, spy: &mut PythonSpy) -> (Vec<StackTrace>, Vec<(Pid, Error)>) {
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
            match spy.resolve_signal_stack(&self.buf[..n]) {
                Ok(trace) => traces.push(trace),
                Err(e) => errors.push((spy.pid, e)),
            }
        }
        let dropped = handler::take_dropped();
        if dropped > 0 {
            debug!("dropped {} samples taken in the signal handler", dropped);
        }
        (traces, errors)
    }
}

impl Drop for SignalHandlerSession {
    /// Stops the timers before the handler is restored: a tick with the default action would
    /// terminate the process. Holding the lock keeps a concurrent hook from using a dead timer.
    fn drop(&mut self) {
        *self.timer.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

/// Starts sampling `tid` if the session runs in [`Mode::CTimer`]. Does nothing otherwise.
pub fn thread_created(timer: &SharedTimer, tid: signal_timer::pid_t) -> Result<(), Error> {
    match timer.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
        Some(timer) => Ok(timer.thread_created(tid)?),
        None => Ok(()),
    }
}

/// Stops sampling `tid`. Does nothing if the session ended or runs in [`Mode::ITimer`].
pub fn thread_removed(timer: &SharedTimer, tid: signal_timer::pid_t) {
    if let Some(timer) = timer.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
        timer.thread_removed(tid);
    }
}
