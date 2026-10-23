use crate::Error;

const WORD: usize = size_of::<usize>();

/// Reads memory of the current process. Implementations must be async-signal-safe.
pub trait Memory {
    /// Reads the word at `addr`.
    fn read(&self, addr: usize) -> Result<usize, Error>;
}

/// Dereferences the address. Fast, but a bad pointer crashes the process.
pub struct Direct {
    _private: (),
}

impl Direct {
    /// # Safety
    /// Every address the unwinder reaches must be readable: the frame chain must be intact.
    pub const unsafe fn new() -> Self {
        Direct { _private: () }
    }
}

impl Memory for Direct {
    fn read(&self, addr: usize) -> Result<usize, Error> {
        if addr < 4096 || addr.checked_add(WORD).is_none() {
            return Err(Error::Read);
        }
        Ok(unsafe { (addr as *const usize).read_unaligned() })
    }
}

/// Copies through the kernel, so an unmapped address fails the read instead of faulting.
/// One syscall per read.
pub struct Checked {
    #[cfg(target_os = "linux")]
    pid: libc::pid_t,
    #[cfg(target_os = "macos")]
    task: libc::mach_port_t,
}

impl Checked {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Checked {
            #[cfg(target_os = "linux")]
            pid: unsafe { libc::getpid() },
            #[cfg(target_os = "macos")]
            task: unsafe { mach_task_self_ },
        }
    }
}

#[cfg(target_os = "linux")]
impl Memory for Checked {
    fn read(&self, addr: usize) -> Result<usize, Error> {
        let mut word: usize = 0;
        let local = libc::iovec {
            iov_base: (&raw mut word).cast(),
            iov_len: WORD,
        };
        let remote = libc::iovec {
            iov_base: addr as *mut libc::c_void,
            iov_len: WORD,
        };
        let n = unsafe { libc::process_vm_readv(self.pid, &local, 1, &remote, 1, 0) };
        if n != WORD as isize {
            return Err(Error::Read);
        }
        Ok(word)
    }
}

#[cfg(target_os = "macos")]
extern "C" {
    static mach_task_self_: libc::mach_port_t;
    fn mach_vm_read_overwrite(
        target_task: libc::mach_port_t,
        address: u64,
        size: u64,
        data: u64,
        outsize: *mut u64,
    ) -> libc::c_int;
}

#[cfg(target_os = "macos")]
impl Memory for Checked {
    fn read(&self, addr: usize) -> Result<usize, Error> {
        let mut word: usize = 0;
        let mut n: u64 = 0;
        let kr = unsafe {
            mach_vm_read_overwrite(
                self.task,
                addr as u64,
                WORD as u64,
                (&raw mut word) as u64,
                &mut n,
            )
        };
        if kr != 0 || n != WORD as u64 {
            return Err(Error::Read);
        }
        Ok(word)
    }
}
