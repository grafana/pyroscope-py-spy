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

    #[cfg(test)]
    mod tests {
        use std::process::Command;

        use remoteprocess::{Pid, Process, ProcessMemory};

        use super::AsanProcessMemory;

        const CHILD_ENV: &str = "PY_SPY_ASAN_CHILD";

        fn own() -> Process {
            Process::new(std::process::id() as Pid).unwrap()
        }

        fn local() -> AsanProcessMemory<Process> {
            AsanProcessMemory::new(own(), std::process::id() as Pid)
        }

        fn is_child() -> bool {
            std::env::var_os(CHILD_ENV).is_some()
        }

        fn run_child(name: &str) -> (String, String) {
            let module = module_path!().split_once("::").unwrap().1;
            let out = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", &format!("{module}::{name}"), "--nocapture"])
                .env(CHILD_ENV, "1")
                .env(
                    "ASAN_OPTIONS",
                    "halt_on_error=0:detect_leaks=0:detect_stack_use_after_return=1",
                )
                .output()
                .unwrap();
            let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
            let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
            assert!(out.status.success(), "child {name} failed:\n{stderr}");
            assert!(
                stdout.contains("child done"),
                "child {name} did not finish:\n{stderr}"
            );
            (stdout, stderr)
        }

        fn assert_report(name: &str, kind: &str, size: usize) {
            let (stdout, stderr) = run_child(name);
            let addr = stdout.lines().find_map(|l| l.strip_prefix("bad ")).unwrap();
            assert_eq!(
                stderr.matches("ERROR: AddressSanitizer").count(),
                1,
                "{stderr}"
            );
            assert!(
                stderr.contains(&format!(
                    "ERROR: AddressSanitizer: {kind} on address {addr} "
                )),
                "{stderr}"
            );
            assert!(
                stderr.contains(&format!("READ of size {size} at {addr} ")),
                "{stderr}"
            );
        }

        fn assert_no_report(name: &str) {
            let (_, stderr) = run_child(name);
            assert!(!stderr.contains("AddressSanitizer"), "{stderr}");
        }

        #[test]
        fn child_valid() {
            if !is_child() {
                return;
            }
            let data = vec![7u8; 32];
            let mut buf = [0u8; 32];
            local().read(data.as_ptr() as usize, &mut buf).unwrap();
            assert_eq!(buf, [7u8; 32]);
            println!("child done");
        }

        #[test]
        fn child_use_after_free() {
            if !is_child() {
                return;
            }
            let addr = Box::into_raw(Box::new([7u8; 64])) as usize;
            drop(unsafe { Box::from_raw(addr as *mut [u8; 64]) });
            println!("bad {addr:#x}");
            let mut buf = [0u8; 64];
            local().read(addr, &mut buf).unwrap();
            println!("child done");
        }

        #[test]
        fn child_buffer_overflow() {
            if !is_child() {
                return;
            }
            let data = vec![7u8; 10];
            println!("bad {:#x}", data.as_ptr() as usize + 10);
            let mut buf = [0u8; 16];
            local().read(data.as_ptr() as usize, &mut buf).unwrap();
            println!("child done");
        }

        #[inline(never)]
        fn returned_stack_addr() -> usize {
            let local = std::hint::black_box([7u8; 64]);
            std::hint::black_box(&local) as *const _ as usize
        }

        #[test]
        fn child_use_after_return() {
            if !is_child() {
                return;
            }
            let addr = returned_stack_addr();
            println!("bad {addr:#x}");
            let mut buf = [0u8; 64];
            local().read(addr, &mut buf).unwrap();
            println!("child done");
        }

        #[test]
        fn child_other_pid() {
            if !is_child() {
                return;
            }
            let addr = Box::into_raw(Box::new([7u8; 64])) as usize;
            drop(unsafe { Box::from_raw(addr as *mut [u8; 64]) });
            let other = AsanProcessMemory::new(own(), std::process::id() as Pid + 1);
            let mut buf = [0u8; 64];
            other.read(addr, &mut buf).unwrap();
            println!("child done");
        }

        #[test]
        fn valid_read_is_not_reported() {
            if is_child() {
                return;
            }
            assert_no_report("child_valid");
        }

        #[test]
        fn reports_use_after_free() {
            if is_child() {
                return;
            }
            assert_report("child_use_after_free", "heap-use-after-free", 64);
        }

        #[test]
        fn reports_buffer_overflow_from_first_poisoned_byte() {
            if is_child() {
                return;
            }
            assert_report("child_buffer_overflow", "heap-buffer-overflow", 6);
        }

        #[test]
        fn reports_use_after_return() {
            if is_child() {
                return;
            }
            assert_report("child_use_after_return", "stack-use-after-return", 64);
        }

        #[test]
        fn other_pid_is_not_reported() {
            if is_child() {
                return;
            }
            assert_no_report("child_other_pid");
        }
    }
}
