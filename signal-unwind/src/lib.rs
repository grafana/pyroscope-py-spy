//! Walks the CPython frame chain of a thread from a signal handler and writes it into a
//! [`signal_ringbuffer`] record. No allocation, no locks, no recursion, no buffers.
//!
//! The handler records only code object addresses and bytecode offsets. Names and line numbers
//! are resolved later on a normal thread, which must check the code object is still alive.
//!
//! Supports CPython 3.14, default (GIL) build, 64-bit targets.

#![cfg_attr(not(test), no_std)]

#[cfg(not(target_pointer_width = "64"))]
compile_error!("signal-unwind supports only 64-bit targets");

#[cfg(test)]
#[allow(clippy::all, unused_imports, unnecessary_transmutes)]
#[path = "../../src/python_bindings/v3_14_0.rs"]
mod v3_14_0;

mod layout;
mod memory;

pub use layout::{Layout, V3_14};
pub use memory::{Checked, Direct, Memory};

use core::marker::PhantomData;
use signal_ringbuffer::Record;

const OB_TYPE: usize = 8;
const WORD: usize = size_of::<usize>();

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// Memory on the chain was unreadable.
    Read,
    /// The ring buffer has no room for the next frame.
    Full,
    /// The chain is longer than `max_frames`, or loops.
    TooDeep,
    /// A pointer on the chain overflows the address space, or an instruction pointer lies
    /// before its code object's bytecode.
    Invalid,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Frame {
    /// Address of the `PyCodeObject`.
    pub code: usize,
    /// Byte offset into the bytecode, as `PyCode_Addr2Line` takes it.
    pub instr_offset: usize,
}

pub struct Unwinder<L, M> {
    memory: M,
    code_type: usize,
    max_frames: usize,
    layout: PhantomData<L>,
}

impl<L: Layout, M: Memory> Unwinder<L, M> {
    /// `code_type` is the address of `PyCode_Type`; frames whose executable has another type are
    /// skipped. `max_frames` bounds the walk, skipped frames included.
    pub const fn new(memory: M, code_type: usize, max_frames: usize) -> Self {
        Unwinder {
            memory,
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
        let mut frame = self.read_at(tstate, L::THREAD_FRAME)?;
        let mut visited = 0;
        let mut pushed = 0;
        while frame != 0 {
            if visited == self.max_frames {
                return Err(Error::TooDeep);
            }
            visited += 1;
            let owner_word = self.read_at(frame, L::OWNER & !(WORD - 1))?;
            let owner = owner_word.to_ne_bytes()[L::OWNER & (WORD - 1)];
            if owner < 8 && L::SKIP_OWNERS & (1 << owner) == 0 {
                let executable = self.read_at(frame, L::EXECUTABLE)? & !L::EXECUTABLE_TAG_BITS;
                if executable != 0 && self.read_at(executable, OB_TYPE)? == self.code_type {
                    let bytecode = executable
                        .checked_add(L::CODE_ADAPTIVE)
                        .ok_or(Error::Invalid)?;
                    let instr_offset = self
                        .read_at(frame, L::INSTR_PTR)?
                        .checked_sub(bytecode)
                        .ok_or(Error::Invalid)?;
                    record
                        .push(Frame {
                            code: executable,
                            instr_offset,
                        })
                        .map_err(|_| Error::Full)?;
                    pushed += 1;
                }
            }
            frame = self.read_at(frame, L::PREVIOUS)?;
        }
        Ok(pushed)
    }

    fn read_at(&self, base: usize, offset: usize) -> Result<usize, Error> {
        self.memory
            .read(base.checked_add(offset).ok_or(Error::Invalid)?)
    }
}
