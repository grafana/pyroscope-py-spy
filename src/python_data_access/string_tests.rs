use super::copy_string;
use crate::python_process_info::{TypeChecks, Types};
use remoteprocess::ProcessMemory;
use std::mem::{offset_of, size_of};

const OBJECT: usize = 0x1000;
const DATA: usize = 0x2000;
const STRING_TYPE: usize = 0x3000;

// Enforce the allocation boundary even without ASAN. Unlike the older 4 KiB
// fixtures, these buffers have exactly CPython's header + payload + terminator.
struct Memory {
    object: usize,
    header: Vec<u8>,
    data: Vec<u8>,
}

impl ProcessMemory for Memory {
    fn read(&self, addr: usize, out: &mut [u8]) -> Result<(), remoteprocess::Error> {
        for (base, bytes) in [(self.object, &self.header), (DATA, &self.data)] {
            if let Some(bytes) = addr
                .checked_sub(base)
                .and_then(|offset| bytes.get(offset..))
                .and_then(|bytes| bytes.get(..out.len()))
            {
                out.copy_from_slice(bytes);
                return Ok(());
            }
        }
        Err(remoteprocess::Error::Other(format!(
            "read outside string allocation: {} bytes at {addr:#x}",
            out.len()
        )))
    }
}

fn put(bytes: &mut [u8], offset: usize, value: &[u8]) {
    bytes[offset..offset + value.len()].copy_from_slice(value);
}

macro_rules! unicode_tests {
    ($py:ident) => {
        mod $py {
            use super::*;
            use crate::python_bindings::$py::{
                PyASCIIObject, PyCompactUnicodeObject, PyUnicodeObject,
            };

            fn memory(text: &str, kind: u32, compact: bool) -> Memory {
                let ascii = text.is_ascii();
                let mut data = Vec::new();
                for ch in text.chars() {
                    match kind {
                        1 => data.push(ch as u8),
                        2 => data.extend_from_slice(&(ch as u16).to_ne_bytes()),
                        4 => data.extend_from_slice(&(ch as u32).to_ne_bytes()),
                        _ => unreachable!(),
                    }
                }
                data.resize(data.len() + kind as usize, 0);
                let header_size = if !compact {
                    size_of::<PyUnicodeObject>()
                } else if ascii {
                    size_of::<PyASCIIObject>()
                } else {
                    size_of::<PyCompactUnicodeObject>()
                };
                let mut header = vec![0; header_size];
                put(
                    &mut header,
                    offset_of!(PyASCIIObject, ob_base.ob_type),
                    &STRING_TYPE.to_ne_bytes(),
                );
                put(
                    &mut header,
                    offset_of!(PyASCIIObject, length),
                    &(text.chars().count() as isize).to_ne_bytes(),
                );
                // These CPython layouts use two interned bits, three kind bits,
                // then the compact and ASCII flags in a four-byte state field.
                let state = (kind << 2) | ((compact as u32) << 5) | ((ascii as u32) << 6);
                put(
                    &mut header,
                    offset_of!(PyASCIIObject, state),
                    &state.to_ne_bytes(),
                );
                if compact {
                    header.append(&mut data);
                } else {
                    put(
                        &mut header,
                        offset_of!(PyUnicodeObject, data),
                        &DATA.to_ne_bytes(),
                    );
                }
                Memory {
                    object: OBJECT,
                    header,
                    data,
                }
            }

            fn check(memory: &Memory, expected: &str) {
                let types = TypeChecks::Strict(Types {
                    code: 0,
                    string: STRING_TYPE,
                    dict: 0,
                });
                assert_eq!(
                    copy_string(memory.object as *const PyUnicodeObject, memory, &types).unwrap(),
                    expected
                );
            }

            #[test]
            fn compact_strings_fit_their_allocations() {
                for (text, kind) in [
                    ("", 1),
                    ("a", 1),
                    ("hash_value", 1),
                    (
                        "a longer ASCII string that fills the full Unicode header",
                        1,
                    ),
                    ("é", 1),
                    ("éé", 1),
                    ("€", 2),
                    ("😀", 4),
                ] {
                    check(&memory(text, kind, true), text);
                }
            }

            #[test]
            fn noncompact_strings_read_external_data() {
                for (text, kind) in [("", 1), ("a", 1), ("é", 1), ("€", 2), ("😀", 4)] {
                    check(&memory(text, kind, false), text);
                }
            }

            #[test]
            fn external_pointer_address_must_not_wrap() {
                let mut memory = memory("a", 1, false);
                memory.header.truncate(size_of::<PyASCIIObject>());
                memory.object = usize::MAX - memory.header.len();
                let error = copy_string(
                    memory.object as *const PyUnicodeObject,
                    &memory,
                    &TypeChecks::Disabled,
                )
                .unwrap_err();
                assert_eq!(error.to_string(), "String header address overflow");
            }
        }
    };
}

unicode_tests!(v3_10_0);
unicode_tests!(v3_11_0);
unicode_tests!(v3_12_0);
unicode_tests!(v3_13_0);
unicode_tests!(v3_14_0);
unicode_tests!(v3_15_0);
