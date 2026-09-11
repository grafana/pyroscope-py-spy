use std::collections::HashMap;
use std::sync::OnceLock;

use anyhow::{Context, Error};

use crate::python_bindings::{
    v3_10_0, v3_11_0, v3_12_0, v3_13_0, v3_14_0, v3_6_6, v3_7_0, v3_8_0, v3_9_5,
};
use crate::python_data_access::{copy_long, copy_string, DictIterator, PY_TPFLAGS_MANAGED_DICT};
use crate::python_interpreters::{InterpreterState, Object, TypeObject};
use crate::python_spy::PythonSpy;
use remoteprocess::Process;

use crate::version::Version;

use remoteprocess::ProcessMemory;

/// Returns a hashmap of threadid: threadname, by inspecting the '_active' variable in the
/// 'threading' module.
pub fn thread_names_from_interpreter<I: InterpreterState, P: ProcessMemory>(
    interpreter_address: usize,
    process: &P,
    version: &Version,
    modules_offset: &OnceLock<usize>,
) -> Result<HashMap<u64, String>, Error> {
    let modules_ptr_ptr = if version.major == 3 && version.minor == 14 {
        let offset = if let Some(offset) = modules_offset.get() {
            *offset
        } else {
            let runtime: usize = process.copy_struct(
                interpreter_address + std::mem::offset_of!(v3_14_0::PyInterpreterState, runtime),
            )?;
            let offsets: v3_14_0::_Py_DebugOffsets = process.copy_struct(runtime)?;
            let offset = offsets.interpreter_state.imports_modules as usize;
            let _ = modules_offset.set(offset);
            offset
        };
        (interpreter_address + offset) as *const *const I::Object
    } else {
        I::modules_ptr_ptr(interpreter_address)
    };
    let modules: *const I::Object = process
        .copy_pointer(modules_ptr_ptr)
        .context("Failed to copy modules PyObject")?;

    let mut ret = HashMap::new();
    for entry in DictIterator::from(process, version, modules as usize)? {
        let (key, value) = entry?;
        let module_name = copy_string(key as *const I::StringObject, process)?;
        if module_name == "threading" {
            let module: I::Object = process.copy_struct(value)?;
            let module_type = process.copy_pointer(module.ob_type())?;
            let dictptr: usize = process.copy_struct(value + module_type.dictoffset() as usize)?;
            for i in DictIterator::from(process, version, dictptr)? {
                let (key, value) = i?;
                let name = copy_string(key as *const I::StringObject, process)?;
                if name == "_active" {
                    for i in DictIterator::from(process, version, value)? {
                        let (key, value) = i?;
                        let (threadid, _) = copy_long(process, version, key)?;

                        let thread: I::Object = process.copy_struct(value)?;
                        let thread_type = process.copy_pointer(thread.ob_type())?;
                        let flags = thread_type.flags();

                        let dict_iter = if flags & PY_TPFLAGS_MANAGED_DICT != 0 {
                            DictIterator::from_managed_dict(
                                process,
                                version,
                                value,
                                thread.ob_type() as usize,
                                flags,
                            )?
                        } else {
                            let dict_offset = thread_type.dictoffset();
                            let dict_addr = (value as isize + dict_offset) as usize;
                            let thread_dict_addr: usize = process.copy_struct(dict_addr)?;
                            DictIterator::from(process, version, thread_dict_addr)?
                        };

                        for i in dict_iter {
                            let (key, value) = i?;
                            let varname = copy_string(key as *const I::StringObject, process)?;
                            if varname == "_name" {
                                let threadname =
                                    copy_string(value as *const I::StringObject, process)?;
                                ret.insert(threadid as u64, threadname);
                                break;
                            }
                        }
                    }
                    break;
                }
            }
            break;
        }
    }
    Ok(ret)
}

/// Returns a hashmap of threadid: threadname, by inspecting the '_active' variable in the
/// 'threading' module.
fn _thread_name_lookup<I: InterpreterState>(
    spy: &PythonSpy,
) -> Result<HashMap<u64, String>, Error> {
    thread_names_from_interpreter::<I, Process>(
        spy.interpreter_address,
        &spy.process,
        &spy.version,
        &spy.python_modules_offset,
    )
}

// try getting the threadnames, but don't sweat it if we can't. Since this relies on dictionary
// processing we only handle py3.6+ right now, and this doesn't work at all if the
// threading module isn't imported in the target program
pub fn thread_name_lookup(process: &PythonSpy) -> Option<HashMap<u64, String>> {
    let err = match process.version {
        Version {
            major: 3, minor: 6, ..
        } => _thread_name_lookup::<v3_6_6::_is>(process),
        Version {
            major: 3, minor: 7, ..
        } => _thread_name_lookup::<v3_7_0::_is>(process),
        Version {
            major: 3, minor: 8, ..
        } => _thread_name_lookup::<v3_8_0::_is>(process),
        Version {
            major: 3, minor: 9, ..
        } => _thread_name_lookup::<v3_9_5::_is>(process),
        Version {
            major: 3,
            minor: 10,
            ..
        } => _thread_name_lookup::<v3_10_0::_is>(process),
        Version {
            major: 3,
            minor: 11,
            ..
        } => _thread_name_lookup::<v3_11_0::_is>(process),
        Version {
            major: 3,
            minor: 12,
            ..
        } => _thread_name_lookup::<v3_12_0::_is>(process),
        Version {
            major: 3,
            minor: 13,
            ..
        } => _thread_name_lookup::<v3_13_0::_is>(process),
        Version {
            major: 3,
            minor: 14,
            ..
        } => _thread_name_lookup::<v3_14_0::_is>(process),
        _ => return None,
    };
    err.ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    struct Memory {
        reads: Cell<usize>,
    }

    impl ProcessMemory for Memory {
        fn read(&self, addr: usize, buf: &mut [u8]) -> Result<(), remoteprocess::Error> {
            let reads = self.reads.get() + 1;
            self.reads.set(reads);
            if reads != 1 {
                if addr == std::mem::offset_of!(v3_14_0::PyInterpreterState, runtime) {
                    buf.copy_from_slice(&0x10000usize.to_ne_bytes());
                    return Ok(());
                }
                if addr == 0x10000 {
                    let mut offsets = v3_14_0::_Py_DebugOffsets::default();
                    offsets.interpreter_state.imports_modules = 0x20000;
                    return remoteprocess::LocalProcess.read(&offsets as *const _ as usize, buf);
                }
            }
            Err(remoteprocess::Error::IOError(
                std::io::Error::from_raw_os_error(libc::EFAULT),
            ))
        }
    }

    #[test]
    fn test_modules_offset_cached_after_successful_read() {
        let process = Memory {
            reads: Cell::new(0),
        };
        let cache = OnceLock::new();
        let version = Version {
            major: 3,
            minor: 14,
            patch: 7,
            release_flags: String::new(),
            build_metadata: None,
        };
        let lookup = |cache| {
            thread_names_from_interpreter::<v3_14_0::PyInterpreterState, _>(
                0, &process, &version, cache,
            )
        };
        assert!(lookup(&cache).is_err());
        assert!(cache.get().is_none());
        assert_eq!(process.reads.get(), 1);
        assert!(lookup(&cache).is_err());
        assert_eq!(cache.get(), Some(&0x20000));
        assert_eq!(process.reads.get(), 4);
        assert!(lookup(&cache).is_err());
        assert_eq!(process.reads.get(), 5);
        assert!(lookup(&OnceLock::new()).is_err());
        assert_eq!(process.reads.get(), 8);
    }
}
