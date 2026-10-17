use std::any::Any;
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::time::{Duration, Instant};

use anyhow::{Context, Error, Result};
use remoteprocess::{Pid, Process, ProcessMemory, Tid};

use crate::config::{LineNo, LockingStrategy};
use crate::python_bindings::{
    v2_7_15, v3_10_0, v3_11_0, v3_12_0, v3_13_0, v3_14_0, v3_3_7, v3_5_5, v3_6_6, v3_7_0, v3_8_0,
    v3_9_5,
};
use crate::python_data_access::{read_bytes_into, read_string_into};
use crate::python_interpreters::{
    CodeObject, FrameObject, InterpreterState, StringObject, ThreadState,
};
use crate::python_process_info::PyTypes;
use crate::python_spy::{is_idle_frame, PythonSpy};
use crate::python_threading::thread_name_lookup;
use crate::stack_trace::{
    copy_pointer, get_gil_threadid, read_struct, unwind_frames_from, RawFrame, RawThread,
    UnwindBuffers,
};
use crate::version::Version;

type FrameOf<I> = <<I as InterpreterState>::ThreadState as ThreadState>::FrameObject;
type CodeOf<I> = <FrameOf<I> as FrameObject>::CodeObject;
type ReadString = fn(usize, &Process, &PyTypes, &mut String) -> Result<(), Error>;

const THREAD_NAME_RETRY: Duration = Duration::from_secs(1);

/// Reusable storage for `PythonSpy::sample`. Keep one per sampling thread: once its
/// vectors have grown to fit the process, sampling stops allocating.
pub struct SampleBuffers {
    unwind: UnwindBuffers,
    frames: Vec<FrameRecord>,
    threads: Vec<ThreadRecord>,
    thread_activity: HashMap<Tid, bool>,
    unnamed_threads: HashSet<u64>,
    unnamed_since: Instant,
    line_table: Vec<u8>,
    name: String,
    filename: String,
}

impl Default for SampleBuffers {
    fn default() -> Self {
        SampleBuffers {
            unwind: UnwindBuffers::default(),
            frames: Vec::new(),
            threads: Vec::new(),
            thread_activity: HashMap::new(),
            unnamed_threads: HashSet::new(),
            unnamed_since: Instant::now(),
            line_table: Vec::new(),
            name: String::new(),
            filename: String::new(),
        }
    }
}

struct FrameRecord {
    code: usize,
    name: usize,
    qualname: Option<usize>,
    filename: usize,
    line: i32,
}

struct ThreadRecord {
    thread_id: u64,
    os_thread_id: Option<u64>,
    owns_gil: bool,
    active: bool,
    frames: Range<usize>,
}

/// The stacks of every sampled thread, borrowed from `SampleBuffers`
pub struct LazySample<'a> {
    spy: &'a PythonSpy,
    bufs: &'a SampleBuffers,
    read_string: ReadString,
}

/// Call stack for a single python thread. Function and file names are read on demand.
pub struct LazyStackTrace<'a> {
    pub pid: Pid,
    pub thread_id: u64,
    pub thread_name: Option<&'a str>,
    pub os_thread_id: Option<u64>,
    pub active: bool,
    pub owns_gil: bool,
    frames: &'a [FrameRecord],
    process: &'a Process,
    types: &'a PyTypes,
    read_string: ReadString,
}

/// A single python frame. `name` and `filename` read the target process again on every call,
/// so they can fail if the code object has been freed since the sample was taken.
pub struct LazyFrame<'a> {
    pub line: i32,
    record: &'a FrameRecord,
    process: &'a Process,
    types: &'a PyTypes,
    read_string: ReadString,
}

impl<'a> LazySample<'a> {
    pub fn len(&self) -> usize {
        self.bufs.threads.len()
    }

    pub fn is_empty(&self) -> bool {
        self.bufs.threads.is_empty()
    }

    pub fn traces(&self) -> impl ExactSizeIterator<Item = LazyStackTrace<'a>> + 'a {
        let (spy, bufs, read_string) = (self.spy, self.bufs, self.read_string);
        bufs.threads.iter().map(move |thread| LazyStackTrace {
            pid: spy.process.pid,
            thread_id: thread.thread_id,
            thread_name: if spy.config.include_thread_names {
                spy.python_thread_names
                    .get(&thread.thread_id)
                    .map(String::as_str)
            } else {
                None
            },
            os_thread_id: thread.os_thread_id,
            active: thread.active,
            owns_gil: thread.owns_gil,
            frames: &bufs.frames[thread.frames.clone()],
            process: &spy.process,
            types: &spy.types,
            read_string,
        })
    }
}

impl<'a> LazyStackTrace<'a> {
    /// Frames from the innermost call outwards, as in `StackTrace::frames`
    pub fn frames(
        &self,
    ) -> impl DoubleEndedIterator<Item = LazyFrame<'a>> + ExactSizeIterator + 'a {
        let (process, types, read_string) = (self.process, self.types, self.read_string);
        self.frames.iter().map(move |record| LazyFrame {
            line: record.line,
            record,
            process,
            types,
            read_string,
        })
    }
}

impl LazyFrame<'_> {
    /// Address of the code object in the target process. Addresses are reused once a code
    /// object is freed, so this is not a stable key for the function on its own.
    pub fn code_address(&self) -> usize {
        self.record.code
    }

    /// Writes the qualified function name (plain name before Python 3.11) into `buf`
    pub fn name(&self, buf: &mut String) -> Result<(), Error> {
        read_name(self.record, self.process, self.types, self.read_string, buf)
    }

    pub fn filename(&self, buf: &mut String) -> Result<(), Error> {
        (self.read_string)(self.record.filename, self.process, self.types, buf)
            .context("Failed to copy filename")
    }
}

fn read_name(
    record: &FrameRecord,
    process: &Process,
    types: &PyTypes,
    read_string: ReadString,
    buf: &mut String,
) -> Result<(), Error> {
    if let Some(qualname) = record.qualname {
        if read_string(qualname, process, types, buf).is_ok() {
            return Ok(());
        }
    }
    read_string(record.name, process, types, buf).context("Failed to copy function name")
}

fn read_remote_string<S: StringObject, P: ProcessMemory>(
    addr: usize,
    process: &P,
    types: &PyTypes,
    out: &mut String,
) -> Result<(), Error> {
    read_string_into(addr as *const S, process, types, out)
}

impl PythonSpy {
    /// Samples every python thread without reading function or file names, reusing `bufs`.
    /// Ignores `native` and `dump_locals`.
    pub fn sample<'a>(&'a mut self, bufs: &'a mut SampleBuffers) -> Result<LazySample<'a>, Error> {
        let (mut walk, _lock) = Walk::begin(self, bufs)?;

        bufs.unwind.frames.clear();
        bufs.unwind.threads.clear();
        while let Some((thread, owns_gil)) = walk.next_raw(self)? {
            let unwind = &mut bufs.unwind;
            let start = unwind.frames.len();
            (walk.ops.unwind)(thread.top_frame, &self.process, &mut unwind.frames).with_context(
                || format!("Failed to unwind frames for thread {}", thread.thread_id),
            )?;
            unwind.threads.push(RawThread {
                thread_id: thread.thread_id,
                os_thread_id: thread.native_thread_id,
                owns_gil,
                frames: start..unwind.frames.len(),
            });
        }

        bufs.frames.clear();
        bufs.threads.clear();
        for i in 0..bufs.unwind.threads.len() {
            let RawThread {
                thread_id,
                os_thread_id,
                owns_gil,
                ref frames,
            } = bufs.unwind.threads[i];
            let raw_frames = frames.clone();
            let os_thread_id = walk.thread_meta(self, bufs, thread_id, os_thread_id)?;
            let (frames, active) = walk.resolve(self, bufs, raw_frames, thread_id, os_thread_id)?;
            bufs.threads.push(ThreadRecord {
                thread_id,
                os_thread_id,
                owns_gil,
                active,
                frames,
            });
        }

        Ok(LazySample {
            spy: self,
            bufs,
            read_string: walk.ops.read_string,
        })
    }

    /// Like `sample`, but reads one thread per `ThreadWalker::next_thread` call and walks its
    /// frames only on `LazyThread::unwind`. Each thread reuses the frame storage of the last.
    pub fn walk_threads<'a>(
        &'a mut self,
        bufs: &'a mut SampleBuffers,
    ) -> Result<ThreadWalker<'a>, Error> {
        let (walk, lock) = Walk::begin(self, bufs)?;
        Ok(ThreadWalker {
            spy: self,
            bufs,
            walk,
            _lock: lock,
        })
    }
}

/// Lending iterator over the python threads of one sample
pub struct ThreadWalker<'a> {
    spy: &'a mut PythonSpy,
    bufs: &'a mut SampleBuffers,
    walk: Walk,
    _lock: Option<Box<dyn Any>>,
}

/// A python thread whose frames have not been read yet
pub struct LazyThread<'w> {
    pub pid: Pid,
    pub thread_id: u64,
    pub os_thread_id: Option<u64>,
    pub owns_gil: bool,
    top_frame: TopFrame,
    spy: &'w mut PythonSpy,
    bufs: &'w mut SampleBuffers,
    walk: &'w Walk,
}

impl ThreadWalker<'_> {
    pub fn next_thread(&mut self) -> Result<Option<LazyThread<'_>>, Error> {
        let Some((thread, owns_gil)) = self.walk.next_raw(self.spy)? else {
            return Ok(None);
        };
        let os_thread_id = self.walk.thread_meta(
            self.spy,
            self.bufs,
            thread.thread_id,
            thread.native_thread_id,
        )?;
        Ok(Some(LazyThread {
            pid: self.spy.process.pid,
            thread_id: thread.thread_id,
            os_thread_id,
            owns_gil,
            top_frame: thread.top_frame,
            spy: self.spy,
            bufs: self.bufs,
            walk: &self.walk,
        }))
    }
}

impl<'w> LazyThread<'w> {
    pub fn thread_name(&self) -> Option<&str> {
        thread_name(self.spy, self.thread_id)
    }

    pub fn unwind(self) -> Result<LazyStackTrace<'w>, Error> {
        let (walk, bufs) = (self.walk, self.bufs);
        bufs.unwind.frames.clear();
        (walk.ops.unwind)(self.top_frame, &self.spy.process, &mut bufs.unwind.frames)
            .with_context(|| format!("Failed to unwind frames for thread {}", self.thread_id))?;
        bufs.frames.clear();
        let raw_frames = 0..bufs.unwind.frames.len();
        let (frames, active) = walk.resolve(
            self.spy,
            bufs,
            raw_frames,
            self.thread_id,
            self.os_thread_id,
        )?;

        let (spy, bufs): (&'w PythonSpy, &'w SampleBuffers) = (self.spy, bufs);
        Ok(LazyStackTrace {
            pid: self.pid,
            thread_id: self.thread_id,
            thread_name: thread_name(spy, self.thread_id),
            os_thread_id: self.os_thread_id,
            active,
            owns_gil: self.owns_gil,
            frames: &bufs.frames[frames],
            process: &spy.process,
            types: &spy.types,
            read_string: walk.ops.read_string,
        })
    }
}

fn thread_name(spy: &PythonSpy, thread_id: u64) -> Option<&str> {
    if !spy.config.include_thread_names {
        return None;
    }
    spy.python_thread_names.get(&thread_id).map(String::as_str)
}

#[derive(Clone, Copy)]
enum TopFrame {
    Direct(usize),
    Indirect(usize),
}

struct ThreadInfo {
    next: usize,
    thread_id: u64,
    native_thread_id: Option<u64>,
    top_frame: TopFrame,
}

type ResolveFrames = fn(
    &[RawFrame],
    &Process,
    LineNo,
    bool,
    &PyTypes,
    &mut Vec<u8>,
    &mut Vec<FrameRecord>,
) -> Result<(), Error>;

/// The version-specific steps of a sample, so that the walk itself is not generic
#[derive(Clone, Copy)]
struct VersionOps {
    threads_head: fn(&Process, usize) -> Result<usize, Error>,
    gil_thread_id: fn(usize, &Process) -> Result<u64, Error>,
    read_thread: fn(&Process, usize) -> Result<ThreadInfo, Error>,
    unwind: fn(TopFrame, &Process, &mut Vec<RawFrame>) -> Result<(), Error>,
    resolve: ResolveFrames,
    os_thread_id: fn(&mut PythonSpy, u64, usize) -> Result<Option<Tid>, Error>,
    read_string: ReadString,
}

impl VersionOps {
    fn new<I: InterpreterState>() -> VersionOps {
        VersionOps {
            threads_head: |process, interpreter| {
                let head: *const I::ThreadState =
                    copy_pointer(process, I::threadstate_ptr_ptr(interpreter))?;
                Ok(head as usize)
            },
            gil_thread_id: get_gil_threadid::<I, Process>,
            read_thread: read_thread::<I::ThreadState>,
            unwind: unwind_from::<FrameOf<I>>,
            resolve: resolve_frame_records::<FrameOf<I>, Process>,
            os_thread_id: |spy, thread_id, head| {
                spy._get_os_thread_id::<I>(thread_id, head as *const I::ThreadState)
            },
            read_string: read_remote_string::<<CodeOf<I> as CodeObject>::StringObject, Process>,
        }
    }

    fn for_version(version: &Version) -> Result<VersionOps, Error> {
        Ok(match version {
            Version {
                major: 2,
                minor: 3..=7,
                ..
            } => Self::new::<v2_7_15::_is>(),
            Version {
                major: 3, minor: 3, ..
            } => Self::new::<v3_3_7::_is>(),
            Version {
                major: 3,
                minor: 4..=5,
                ..
            } => Self::new::<v3_5_5::_is>(),
            Version {
                major: 3, minor: 6, ..
            } => Self::new::<v3_6_6::_is>(),
            Version {
                major: 3, minor: 7, ..
            } => Self::new::<v3_7_0::_is>(),
            Version {
                major: 3, minor: 8, ..
            } => Self::new::<v3_8_0::_is>(),
            Version {
                major: 3, minor: 9, ..
            } => Self::new::<v3_9_5::_is>(),
            Version {
                major: 3,
                minor: 10,
                ..
            } => Self::new::<v3_10_0::_is>(),
            Version {
                major: 3,
                minor: 11,
                ..
            } => Self::new::<v3_11_0::_is>(),
            Version {
                major: 3,
                minor: 12,
                ..
            } => Self::new::<v3_12_0::_is>(),
            Version {
                major: 3,
                minor: 13,
                ..
            } => Self::new::<v3_13_0::_is>(),
            Version {
                major: 3,
                minor: 14,
                ..
            } => Self::new::<v3_14_0::_is>(),
            _ => return Err(format_err!("Unsupported version of Python: {}", version)),
        })
    }
}

fn read_thread<T: ThreadState>(process: &Process, addr: usize) -> Result<ThreadInfo, Error> {
    let thread = copy_pointer(process, addr as *const T)?;
    let top_frame = match thread.frame_address() {
        Some(addr) => TopFrame::Indirect(addr),
        None => TopFrame::Direct(thread.frame(None) as usize),
    };
    Ok(ThreadInfo {
        next: thread.next() as usize,
        thread_id: thread.thread_id(),
        native_thread_id: thread.native_thread_id(),
        top_frame,
    })
}

// ThreadState::frame(Some(addr)) is `addr` on every version that has a frame_address
fn unwind_from<F: FrameObject>(
    top_frame: TopFrame,
    process: &Process,
    out: &mut Vec<RawFrame>,
) -> Result<(), Error> {
    let frame = match top_frame {
        TopFrame::Direct(frame) => frame,
        TopFrame::Indirect(addr) => read_struct::<usize, Process>(process, addr)?,
    };
    unwind_frames_from(frame as *const F, process, out)
}

struct Walk {
    ops: VersionOps,
    head: usize,
    next: usize,
    walked: usize,
    gil_thread_id: u64,
    check_shims: bool,
    names_looked_up: bool,
}

impl Walk {
    fn begin(
        spy: &mut PythonSpy,
        bufs: &mut SampleBuffers,
    ) -> Result<(Walk, Option<Box<dyn Any>>), Error> {
        let ops = VersionOps::for_version(&spy.version)?;

        bufs.thread_activity.clear();
        if !spy.config.gil_only {
            for thread in spy.process.threads()?.iter() {
                let threadid: Tid = thread.id()?;
                let Ok(active) = thread.active() else {
                    continue;
                };
                bufs.thread_activity.insert(threadid, active);
            }
        }

        let lock: Option<Box<dyn Any>> = if spy.config.blocking == LockingStrategy::Lock {
            Some(Box::new(
                spy.process.lock().context("Failed to suspend process")?,
            ))
        } else {
            None
        };

        let head = (ops.threads_head)(&spy.process, spy.interpreter_address)
            .context("Failed to copy PyThreadState head pointer")?;
        let gil_thread_id = (ops.gil_thread_id)(spy.threadstate_address, &spy.process)
            .context("failed to get gil_thread_id")?;

        if bufs.unnamed_since.elapsed() >= THREAD_NAME_RETRY {
            bufs.unnamed_threads.clear();
            bufs.unnamed_since = Instant::now();
        }

        let walk = Walk {
            ops,
            head,
            next: head,
            walked: 0,
            gil_thread_id,
            check_shims: matches!(
                spy.version,
                Version {
                    major: 3,
                    minor: 13..,
                    ..
                }
            ),
            names_looked_up: false,
        };
        Ok((walk, lock))
    }

    fn next_raw(&mut self, spy: &PythonSpy) -> Result<Option<(ThreadInfo, bool)>, Error> {
        while self.next != 0 {
            self.walked += 1;
            if self.walked > 4096 {
                self.next = 0;
                return Err(format_err!("Max thread recursion depth reached"));
            }

            let addr = std::mem::replace(&mut self.next, 0);
            let thread = (self.ops.read_thread)(&spy.process, addr)
                .context("Failed to copy PyThreadState")?;
            self.next = thread.next;

            let owns_gil = thread.thread_id == self.gil_thread_id;
            if spy.config.gil_only {
                if !owns_gil {
                    continue;
                }
                self.next = 0;
            }
            return Ok(Some((thread, owns_gil)));
        }
        Ok(None)
    }

    fn thread_meta(
        &mut self,
        spy: &mut PythonSpy,
        bufs: &mut SampleBuffers,
        thread_id: u64,
        native_thread_id: Option<u64>,
    ) -> Result<Option<u64>, Error> {
        let mut os_thread_id = native_thread_id;
        if os_thread_id.is_none() {
            let mut tid = (self.ops.os_thread_id)(spy, thread_id, self.head)?;
            if let Some(id) = tid {
                if !bufs.thread_activity.is_empty() && !bufs.thread_activity.contains_key(&id) {
                    info!("clearing away thread id caches, thread {} has exited", id);
                    spy.python_thread_ids.clear();
                    spy.python_thread_names.clear();
                    bufs.unnamed_threads.clear();
                    tid = (self.ops.os_thread_id)(spy, thread_id, self.head)?;
                }
            }
            os_thread_id = tid.map(|id| id as u64);
        }

        // The lookup walks sys.modules, so retry threads it could not name at most once a second
        if spy.config.include_thread_names
            && !spy.python_thread_names.contains_key(&thread_id)
            && !bufs.unnamed_threads.contains(&thread_id)
        {
            if !self.names_looked_up {
                spy.python_thread_names = thread_name_lookup(spy).unwrap_or_default();
                self.names_looked_up = true;
            }
            if !spy.python_thread_names.contains_key(&thread_id) {
                bufs.unnamed_threads.insert(thread_id);
            }
        }
        Ok(os_thread_id)
    }

    fn resolve(
        &self,
        spy: &PythonSpy,
        bufs: &mut SampleBuffers,
        raw_frames: Range<usize>,
        thread_id: u64,
        os_thread_id: Option<u64>,
    ) -> Result<(Range<usize>, bool), Error> {
        let start = bufs.frames.len();
        (self.ops.resolve)(
            &bufs.unwind.frames[raw_frames],
            &spy.process,
            spy.config.lineno,
            self.check_shims,
            &spy.types,
            &mut bufs.line_table,
            &mut bufs.frames,
        )
        .with_context(|| format!("Failed to resolve frames for thread {}", thread_id))?;
        let frames = start..bufs.frames.len();

        let mut active = os_thread_id
            .and_then(|id| bufs.thread_activity.get(&(id as Tid)).copied())
            .unwrap_or(true);
        if let (true, Some(top)) = (active, bufs.frames[frames.clone()].first()) {
            let read_string = self.ops.read_string;
            let resolved = read_name(top, &spy.process, &spy.types, read_string, &mut bufs.name)
                .and_then(|()| {
                    read_string(top.filename, &spy.process, &spy.types, &mut bufs.filename)
                });
            active = !(resolved.is_ok() && is_idle_frame(&bufs.name, &bufs.filename));
        }
        Ok((frames, active))
    }
}

fn resolve_frame_records<F: FrameObject, P: ProcessMemory>(
    raw: &[RawFrame],
    process: &P,
    lineno: LineNo,
    check_shims: bool,
    types: &PyTypes,
    line_table: &mut Vec<u8>,
    out: &mut Vec<FrameRecord>,
) -> Result<(), Error> {
    for raw in raw {
        let code = copy_pointer(process, raw.code as *const F::CodeObject)
            .context("Failed to copy PyCodeObject")?;

        // f_code can be torn or already freed when sampling without pausing the process.
        if code.ob_type() != types.code {
            continue;
        }
        if check_shims && is_shim_filename(code.filename(), process, types) {
            continue;
        }

        let line = match lineno {
            LineNo::NoLine => 0,
            LineNo::First => code.first_lineno(),
            LineNo::LastInstruction => {
                match read_bytes_into(code.line_table(), process, line_table) {
                    Ok(()) => code.get_line_number(raw.lasti, line_table),
                    Err(e) => {
                        warn!(
                            "Failed to get line number for code object 0x{:x}: {}",
                            raw.code, e
                        );
                        0
                    }
                }
            }
        };

        out.push(FrameRecord {
            code: raw.code,
            name: code.name() as usize,
            qualname: code.qualname().map(|ptr| ptr as usize),
            filename: code.filename() as usize,
            line,
        });
    }
    Ok(())
}

/// Matches the frames `resolve_frames` drops for an unreadable, empty or `<shim>` filename,
/// reading only the string header unless the length is that of `<shim>`.
fn is_shim_filename<S: StringObject, P: ProcessMemory>(
    ptr: *const S,
    process: &P,
    types: &PyTypes,
) -> bool {
    const SHIM: &[u8] = b"<shim>";
    let Ok(obj) = copy_pointer(process, ptr) else {
        return true;
    };
    if obj.ob_type() != types.string {
        return true;
    }
    match obj.size() {
        Ok(0) => true,
        Ok(len) if len == SHIM.len() && obj.ascii() => {
            let mut buf = [0u8; SHIM.len()];
            match obj.address(ptr as usize) {
                Ok(addr) => process.read(addr, &mut buf).is_err() || buf == SHIM,
                Err(_) => true,
            }
        }
        Ok(_) => false,
        Err(_) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::python_bindings::v3_7_0::{self, PyCodeObject};
    use crate::python_data_access::tests::to_asciiobject;
    use remoteprocess::LocalProcess;

    const CODE_TYPE: usize = 0x1000;
    const STRING_TYPE: usize = 0x2000;
    const TYPES: PyTypes = PyTypes {
        code: CODE_TYPE,
        string: STRING_TYPE,
    };

    fn records_for(filenames: &[&str], check_shims: bool) -> Vec<FrameRecord> {
        let mut strings: Vec<_> = filenames.iter().map(|f| to_asciiobject(f)).collect();
        let mut codes: Vec<_> = strings
            .iter_mut()
            .map(|s| {
                s.base.ob_base.ob_type = STRING_TYPE as *mut v3_7_0::_typeobject;
                PyCodeObject {
                    ob_base: v3_7_0::PyObject {
                        ob_type: CODE_TYPE as *mut v3_7_0::_typeobject,
                        ..Default::default()
                    },
                    co_filename: &mut s.base as *mut _ as *mut v3_7_0::PyObject,
                    co_name: &mut s.base as *mut _ as *mut v3_7_0::PyObject,
                    co_firstlineno: 7,
                    ..Default::default()
                }
            })
            .collect();
        let raw: Vec<_> = codes
            .iter_mut()
            .map(|code| RawFrame {
                frame: 0,
                code: code as *mut _ as usize,
                lasti: 0,
                is_entry: false,
            })
            .collect();
        let mut out = Vec::new();
        resolve_frame_records::<v3_7_0::PyFrameObject, _>(
            &raw,
            &LocalProcess,
            LineNo::First,
            check_shims,
            &TYPES,
            &mut Vec::new(),
            &mut out,
        )
        .unwrap();
        out
    }

    #[test]
    fn test_shim_frames_are_skipped() {
        assert_eq!(records_for(&["a.py", "<shim>", "", "b.py"], true).len(), 2);
        assert_eq!(records_for(&["a.py", "<shim>", "", "b.py"], false).len(), 4);
    }

    #[test]
    fn test_first_lineno() {
        let records = records_for(&["test.py"], true);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].line, 7);
    }

    #[test]
    fn test_read_string_reuses_buffer() {
        let mut filename = to_asciiobject("test.py");
        filename.base.ob_base.ob_type = STRING_TYPE as *mut v3_7_0::_typeobject;
        let ptr = &filename.base as *const _ as usize;

        let mut buf = String::with_capacity(64);
        let data = buf.as_ptr();
        read_remote_string::<v3_7_0::PyUnicodeObject, _>(ptr, &LocalProcess, &TYPES, &mut buf)
            .unwrap();
        assert_eq!(buf, "test.py");
        assert_eq!(buf.as_ptr(), data);

        let wrong = PyTypes {
            code: CODE_TYPE,
            string: STRING_TYPE + 0x20,
        };
        assert!(read_remote_string::<v3_7_0::PyUnicodeObject, _>(
            ptr,
            &LocalProcess,
            &wrong,
            &mut buf
        )
        .is_err());
        assert!(buf.is_empty());
    }
}
