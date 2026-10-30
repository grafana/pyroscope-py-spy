//! Walks the CPython frame chain of a thread from a signal handler and writes it into a
//! [`ringbuffer`](super::ringbuffer) record. No allocation, no locks, no recursion, no buffers.
//!
//! The handler records only code object addresses. Names are resolved later on a normal thread,
//! which must check the code object is still alive. Line numbers are not recorded.
//!
//! [`handler`] has ready-made `SIGPROF` handlers, one per layout.
//!
//! Supports CPython 3.11 to 3.14, default (GIL) build, 64-bit targets.

pub mod handler;
mod layout;

pub use kindasafe_init::init;
pub use layout::{Layout, V3_11, V3_12, V3_13, V3_14};

use super::ringbuffer::Record;
use core::marker::PhantomData;

pub const OB_TYPE: usize = 8;
const WORD: usize = size_of::<usize>();

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// Memory on the chain was unreadable.
    Read,
    /// The ring buffer has no room for the next frame.
    Full,
    /// The chain is longer than `max_frames`, or loops.
    TooDeep,
    /// A pointer on the chain overflows the address space.
    Invalid,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Frame {
    /// Address of the `PyCodeObject`.
    pub code: usize,
}

pub struct Unwinder<L> {
    code_type: usize,
    max_frames: usize,
    layout: PhantomData<L>,
}

impl<L: Layout> Unwinder<L> {
    /// [`init`] must have succeeded first, or an unreadable address crashes the process.
    ///
    /// `code_type` is the address of `PyCode_Type`; frames whose executable has another type are
    /// skipped. `max_frames` bounds the walk, skipped frames included.
    pub const fn new(code_type: usize, max_frames: usize) -> Self {
        Unwinder {
            code_type,
            max_frames,
            layout: PhantomData,
        }
    }

    /// Pushes a [`Frame`] for each Python frame of `tstate`, innermost first, and
    /// returns the number of frames. On error the frames pushed so far stay in `record`; the
    /// caller decides whether to commit them.
    pub fn unwind<const N: usize>(
        &self,
        tstate: usize,
        record: &mut Record<'_, '_, Frame, N>,
    ) -> Result<usize, Error> {
        let mut frame = match L::CFRAME_FRAME {
            Some(offset) => {
                let cframe = self.read_at(tstate, L::THREAD_FRAME)?;
                self.read_at(cframe, offset)?
            }
            None => self.read_at(tstate, L::THREAD_FRAME)?,
        };
        let mut visited = 0;
        let mut pushed = 0;
        while frame != 0 {
            if visited == self.max_frames {
                return Err(Error::TooDeep);
            }
            visited += 1;
            let is_python = match L::OWNER {
                Some(offset) => {
                    let owner_word = self.read_at(frame, offset & !(WORD - 1))?;
                    let owner = owner_word.to_ne_bytes()[offset & (WORD - 1)];
                    owner < 8 && L::SKIP_OWNERS & (1 << owner) == 0
                }
                None => true,
            };
            if is_python {
                let executable = self.read_at(frame, L::EXECUTABLE)? & !L::EXECUTABLE_TAG_BITS;
                if executable != 0 && self.read_at(executable, OB_TYPE)? == self.code_type {
                    record
                        .push(Frame { code: executable })
                        .map_err(|_| Error::Full)?;
                    pushed += 1;
                }
            }
            frame = self.read_at(frame, L::PREVIOUS)?;
        }
        Ok(pushed)
    }

    fn read_at(&self, base: usize, offset: usize) -> Result<usize, Error> {
        let addr = base.checked_add(offset).ok_or(Error::Invalid)?;
        kindasafe::u64(addr as u64)
            .map(|word| word as usize)
            .map_err(|_| Error::Read)
    }
}
