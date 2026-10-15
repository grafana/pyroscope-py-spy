#[cfg(not(py_spy_asan))]
pub type SpyProcess = remoteprocess::Process;

#[cfg(not(py_spy_asan))]
pub fn wrap(process: remoteprocess::Process) -> SpyProcess {
    process
}

#[cfg(py_spy_asan)]
pub use imp::*;

#[cfg(py_spy_asan)]
mod imp {
    use std::ops::Deref;

    use remoteprocess::{Error, Pid, Process, ProcessMemory};

    pub type SpyProcess = AsanProcessMemory<Process>;

    pub fn wrap(process: Process) -> SpyProcess {
        let pid = process.pid;
        AsanProcessMemory::new(process, pid)
    }

    /// Reports reads of our own address space that hit memory ASAN considers
    /// poisoned, which reads through `process_vm_readv` would otherwise hide.
    pub struct AsanProcessMemory<P> {
        inner: P,
        local: bool,
    }

    impl<P> AsanProcessMemory<P> {
        pub fn new(inner: P, pid: Pid) -> Self {
            AsanProcessMemory {
                inner,
                local: pid as u32 == std::process::id(),
            }
        }
    }

    impl<P> Deref for AsanProcessMemory<P> {
        type Target = P;

        fn deref(&self) -> &P {
            &self.inner
        }
    }

    impl<P: ProcessMemory> ProcessMemory for AsanProcessMemory<P> {
        fn read(&self, addr: usize, buf: &mut [u8]) -> Result<(), Error> {
            self.inner.read(addr, buf)?;
            if self.local {
                report_poisoned_load(addr, buf.len());
            }
            Ok(())
        }
    }

    extern "C" {
        fn __asan_region_is_poisoned(beg: *const u8, size: usize) -> *const u8;
        fn __asan_report_load_n_noabort(addr: *const u8, size: usize);
    }

    fn report_poisoned_load(addr: usize, len: usize) {
        unsafe {
            let bad = __asan_region_is_poisoned(addr as *const u8, len);
            if !bad.is_null() {
                __asan_report_load_n_noabort(bad, addr + len - bad as usize);
            }
        }
    }
}
