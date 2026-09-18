#![allow(clippy::unnecessary_cast)]
use anyhow::Error;

use crate::python_bindings::v3_13_0;
use crate::python_interpreters::{
    BytesObject, InterpreterState, ListObject, Object, StringObject, TupleObject, TypeObject,
};
use crate::utils::offset_of;
use crate::version::Version;
use remoteprocess::ProcessMemory;

/// Bytes read out of the target process aren't a well formed string. Its own type so callers
/// can tell this apart from an ordinary read failure: only this one discards the sample.
#[derive(Debug)]
pub struct InvalidString;

impl std::fmt::Display for InvalidString {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "invalid string read from target process")
    }
}

impl std::error::Error for InvalidString {}

/// Walks the chain rather than downcasting: every call site wraps the error in `.context(..)`,
/// so an outermost-only check would silently disable the option.
pub fn is_invalid_string(err: &Error) -> bool {
    err.chain().any(|cause| cause.is::<InvalidString>())
}

/// Copies a string from a target process. Attempts to handle unicode differences, which mostly seems to be working
///
/// `check_utf8` rejects anything inconsistent with the string's declared kind instead of
/// substituting replacement characters. The `kind`/`ascii` bits below come from the target too,
/// so bad data means this isn't the `PyUnicodeObject` we thought it was.
pub fn copy_string<T: StringObject, P: ProcessMemory>(
    ptr: *const T,
    process: &P,
    check_utf8: bool,
) -> Result<String, Error> {
    let obj = process.copy_pointer(ptr)?;
    if obj.size() == 0 {
        return Ok(String::new());
    }
    if obj.size() >= 4096 {
        return Err(format_err!(
            "Refusing to copy {} chars of a string",
            obj.size()
        ));
    }

    let kind = obj.kind();

    let bytes = process.copy(obj.address(ptr as usize), obj.size() * kind as usize)?;

    match (kind, obj.ascii()) {
        (4, _) => {
            // don't reinterpret these as `char`: a char has to be a unicode scalar value, and
            // these words are whatever the target happened to have in memory
            let mut ret = String::with_capacity(bytes.len());
            for chunk in bytes.chunks_exact(4) {
                let cp = u32::from_ne_bytes(chunk.try_into().unwrap());
                match char::from_u32(cp) {
                    Some(c) => ret.push(c),
                    None if check_utf8 => return Err(InvalidString.into()),
                    None => ret.push(char::REPLACEMENT_CHARACTER),
                }
            }
            Ok(ret)
        }
        (2, _) => {
            let units: Vec<u16> = bytes
                .chunks_exact(2)
                .map(|chunk| u16::from_ne_bytes(chunk.try_into().unwrap()))
                .collect();
            match String::from_utf16(&units) {
                Ok(s) => Ok(s),
                Err(_) if check_utf8 => Err(InvalidString.into()),
                Err(e) => Err(e.into()),
            }
        }
        (1, true) => {
            // stricter than from_utf8 on purpose: a PyASCIIObject can't hold the multibyte
            // sequences from_utf8 would accept
            if check_utf8 && bytes.iter().any(|&b| b >= 0x80) {
                return Err(InvalidString.into());
            }
            match String::from_utf8(bytes) {
                Ok(s) => Ok(s),
                Err(_) if check_utf8 => Err(InvalidString.into()),
                Err(e) => Err(e.into()),
            }
        }
        // latin-1, so widening is the correct decoding and can't fail
        (1, false) => Ok(bytes.iter().map(|&b| b as char).collect()),
        _ => Err(format_err!("Unknown string kind {}", kind)),
    }
}

/// Copies data from a PyBytesObject (currently only lnotab object)
pub fn copy_bytes<T: BytesObject, P: ProcessMemory>(
    ptr: *const T,
    process: &P,
) -> Result<Vec<u8>, Error> {
    let obj = process.copy_pointer(ptr)?;
    let size = obj.size();
    if size >= 65536 {
        return Err(format_err!("Refusing to copy {} bytes", size));
    }
    Ok(process.copy(obj.address(ptr as usize), size as usize)?)
}

/// Copies a i64 from a PyLongObject. Returns the value + if it overflowed
pub fn copy_long<P: ProcessMemory>(
    process: &P,
    version: &Version,
    addr: usize,
) -> Result<(i64, bool), Error> {
    let (size, negative, digit, value_size) = match version {
        Version {
            major: 3,
            minor: 12..=14,
            ..
        } => {
            // PyLongObject format changed in python 3.12
            let value = process
                .copy_pointer(addr as *const crate::python_bindings::v3_12_0::PyLongObject)?;
            let size = value.long_value.lv_tag >> 3;
            let negative: i64 = if (value.long_value.lv_tag & 3) == 2 {
                -1
            } else {
                1
            };
            (
                size,
                negative,
                value.long_value.ob_digit[0] as u32,
                std::mem::size_of_val(&value),
            )
        }
        _ => {
            // this is PyLongObject for a specific version of python, but this works since it's binary compatible
            // layout across versions we're targeting
            let value = process
                .copy_pointer(addr as *const crate::python_bindings::v3_7_0::PyLongObject)?;
            let negative: i64 = if value.ob_base.ob_size < 0 { -1 } else { 1 };
            let size = (value.ob_base.ob_size * (negative as isize)) as usize;
            (
                size,
                negative,
                value.ob_digit[0] as u32,
                std::mem::size_of_val(&value),
            )
        }
    };

    match size {
        0 => Ok((0, false)),
        1 => Ok((negative * (digit as i64), false)),

        #[cfg(target_pointer_width = "64")]
        2 => {
            let digits: [u32; 2] = process.copy_struct(addr + value_size - 8)?;
            let mut ret: i64 = 0;
            for i in 0..size {
                ret += (digits[i as usize] as i64) << (30 * i);
            }
            Ok((negative * ret, false))
        }
        #[cfg(target_pointer_width = "32")]
        2..=4 => {
            let digits: [u16; 4] = process.copy_struct(addr + value_size - 4)?;
            let mut ret: i64 = 0;
            for i in 0..size {
                ret += (digits[i as usize] as i64) << (15 * i);
            }
            Ok((negative * ret, false))
        }
        // we don't support arbitrary sized integers yet, signal this by returning that we've overflowed
        _ => Ok((size as i64, true)),
    }
}

/// Copies a i64 from a python 2.7 PyIntObject
pub fn copy_int<P: ProcessMemory>(process: &P, addr: usize) -> Result<i64, Error> {
    let value =
        process.copy_pointer(addr as *const crate::python_bindings::v2_7_15::PyIntObject)?;
    Ok(value.ob_ival as i64)
}

/// Allows iteration of a python dictionary. Only supports python 3.6+ right now
pub struct DictIterator<'a, P: 'a> {
    process: &'a P,
    entries_addr: usize,
    kind: u8,
    index: usize,
    entries: usize,
    values: usize,
}

impl<'a, P: ProcessMemory> DictIterator<'a, P> {
    pub fn from_managed_dict(
        process: &'a P,
        version: &'a Version,
        addr: usize,
        tp_addr: usize,
        flags: usize,
    ) -> Result<DictIterator<'a, P>, Error> {
        // Handles logic of _PyObject_ManagedDictPointer in python 3.11
        let mut values_addr: usize =
            process.copy_struct(addr - 4 * std::mem::size_of::<usize>())?;

        // TODO: MANAGED_DICT_OFFSET is -3 if GIL isn't disabled, -1 otherwise (in py3.13+)
        // handle gil-less python branches
        let mut dict_addr: usize = process.copy_struct(addr - 3 * std::mem::size_of::<usize>())?;

        // for python 3.12, the values/dict are combined into a single tagged pointer
        if version.major == 3 && version.minor == 12 {
            if dict_addr & 1 == 0 {
                values_addr = 0;
            } else {
                values_addr = dict_addr + 1;
                dict_addr = 0;
            }
        }

        if values_addr != 0 {
            let ht_cached_keys = if version.major == 3 && version.minor >= 12 {
                let ht: crate::python_bindings::v3_12_0::PyHeapTypeObject =
                    process.copy_struct(tp_addr)?;
                ht.ht_cached_keys as usize
            } else {
                let ht: crate::python_bindings::v3_11_0::PyHeapTypeObject =
                    process.copy_struct(tp_addr)?;
                ht.ht_cached_keys as usize
            };

            // handle inline values in py3.13+
            // https://github.com/python/cpython/issues/115776
            if flags & PY_TPFLAGS_INLINE_VALUES != 0 {
                // PyDictValues is stored inline after the initial PyObject
                let dict_values: v3_13_0::_dictvalues = Default::default();
                let values_offset = offset_of(&dict_values, &dict_values.values);

                values_addr = addr + std::mem::size_of::<v3_13_0::PyObject>() + values_offset;
            }

            let keys: crate::python_bindings::v3_12_0::PyDictKeysObject =
                process.copy_struct(ht_cached_keys as usize)?;

            let entries_addr = ht_cached_keys as usize
                + (1 << keys.dk_log2_index_bytes)
                + std::mem::size_of_val(&keys);
            Ok(DictIterator {
                process,
                entries_addr,
                index: 0,
                kind: keys.dk_kind,
                entries: keys.dk_nentries as usize,
                values: values_addr,
            })
        } else if dict_addr != 0 {
            DictIterator::from(process, version, dict_addr)
        } else {
            Err(format_err!("neither values or dict is set in managed dict"))
        }
    }

    pub fn from(
        process: &'a P,
        version: &'a Version,
        addr: usize,
    ) -> Result<DictIterator<'a, P>, Error> {
        match version {
            Version {
                major: 3,
                minor: 11..=14,
                ..
            } => {
                let dict: crate::python_bindings::v3_11_0::PyDictObject =
                    process.copy_struct(addr)?;
                let keys = process.copy_pointer(dict.ma_keys)?;

                let entries_addr = dict.ma_keys as usize
                    + (1 << keys.dk_log2_index_bytes)
                    + std::mem::size_of_val(&keys);
                Ok(DictIterator {
                    process,
                    entries_addr,
                    index: 0,
                    kind: keys.dk_kind,
                    entries: keys.dk_nentries as usize,
                    values: dict.ma_values as usize,
                })
            }
            _ => {
                let dict: crate::python_bindings::v3_7_0::PyDictObject =
                    process.copy_struct(addr)?;
                // Getting this going generically is tricky: there is a lot of variation on how dictionaries are handled
                // instead this just focuses on a single version, which works for python
                // 3.6/3.7/3.8/3.9/3.10
                let keys = process.copy_pointer(dict.ma_keys)?;
                let index_size = match keys.dk_size {
                    0..=0xff => 1,
                    0..=0xffff => 2,
                    #[cfg(target_pointer_width = "64")]
                    0..=0xffffffff => 4,
                    #[cfg(target_pointer_width = "64")]
                    _ => 8,
                    #[cfg(not(target_pointer_width = "64"))]
                    _ => 4,
                };
                let byteoffset = (keys.dk_size * index_size) as usize;
                let entries_addr =
                    dict.ma_keys as usize + byteoffset + std::mem::size_of_val(&keys);
                Ok(DictIterator {
                    process,
                    entries_addr,
                    index: 0,
                    kind: 0,
                    entries: keys.dk_nentries as usize,
                    values: dict.ma_values as usize,
                })
            }
        }
    }
}

impl<'a, P: ProcessMemory> Iterator for DictIterator<'a, P> {
    type Item = Result<(usize, usize), Error>;

    fn next(&mut self) -> Option<Self::Item> {
        while self.index < self.entries {
            let index = self.index;
            self.index += 1;

            // get the addresses of the key/value for the current index
            let entry = match self.kind {
                0 => {
                    let addr = index
                        * std::mem::size_of::<crate::python_bindings::v3_7_0::PyDictKeyEntry>()
                        + self.entries_addr;
                    let ret = self
                        .process
                        .copy_struct::<crate::python_bindings::v3_7_0::PyDictKeyEntry>(addr);
                    ret.map(|entry| (entry.me_key as usize, entry.me_value as usize))
                }
                _ => {
                    // Python 3.11 added a PyDictUnicodeEntry , which uses the hash from the Unicode key rather than recalculate
                    let addr = index
                        * std::mem::size_of::<crate::python_bindings::v3_11_0::PyDictUnicodeEntry>(
                        )
                        + self.entries_addr;
                    let ret = self
                        .process
                        .copy_struct::<crate::python_bindings::v3_11_0::PyDictUnicodeEntry>(addr);
                    ret.map(|entry| (entry.me_key as usize, entry.me_value as usize))
                }
            };

            match entry {
                Ok((key, value)) => {
                    if key == 0 {
                        continue;
                    }

                    let value = if self.values != 0 {
                        let valueaddr = self.values
                            + index
                                * std::mem::size_of::<*mut crate::python_bindings::v3_7_0::PyObject>(
                                );
                        match self.process.copy_struct(valueaddr) {
                            Ok(addr) => addr,
                            Err(e) => {
                                return Some(Err(e.into()));
                            }
                        }
                    } else {
                        value
                    };

                    return Some(Ok((key, value)));
                }
                Err(e) => return Some(Err(e.into())),
            }
        }

        None
    }
}

pub const PY_TPFLAGS_INLINE_VALUES: usize = 1 << 2;
pub const PY_TPFLAGS_MANAGED_DICT: usize = 1 << 4;
const PY_TPFLAGS_INT_SUBCLASS: usize = 1 << 23;
const PY_TPFLAGS_LONG_SUBCLASS: usize = 1 << 24;
const PY_TPFLAGS_LIST_SUBCLASS: usize = 1 << 25;
const PY_TPFLAGS_TUPLE_SUBCLASS: usize = 1 << 26;
const PY_TPFLAGS_BYTES_SUBCLASS: usize = 1 << 27;
const PY_TPFLAGS_STRING_SUBCLASS: usize = 1 << 28;
const PY_TPFLAGS_DICT_SUBCLASS: usize = 1 << 29;

/// Converts a python variable in the other process to a human readable string
pub fn format_variable<I, P>(
    process: &P,
    version: &Version,
    addr: usize,
    max_length: isize,
) -> Result<String, Error>
where
    I: InterpreterState,
    P: ProcessMemory,
{
    // We need at least 5 characters remaining for all this code to work, replace with an ellipsis if
    // we're out of space
    if max_length <= 5 {
        return Ok("...".to_owned());
    }

    let value: I::Object = process.copy_struct(addr)?;
    let value_type = process.copy_pointer(value.ob_type())?;

    // get the typename (truncating to 128 bytes if longer)
    let max_type_len = 128;
    let value_type_name = process.copy(value_type.name() as usize, max_type_len)?;
    let length = value_type_name
        .iter()
        .position(|&x| x == 0)
        .unwrap_or(max_type_len);
    let value_type_name = std::str::from_utf8(&value_type_name[..length])?;

    let format_int = |value: i64| {
        if value_type_name == "bool" {
            (if value > 0 { "True" } else { "False" }).to_owned()
        } else {
            format!("{value}")
        }
    };

    // use the flags/typename to figure out how to stringify this object
    let flags = value_type.flags();
    let formatted = if flags & PY_TPFLAGS_INT_SUBCLASS != 0 {
        format_int(copy_int(process, addr)?)
    } else if flags & PY_TPFLAGS_LONG_SUBCLASS != 0 {
        // we don't handle arbitrary sized integer values (max is 2**60)
        let (value, overflowed) = copy_long(process, version, addr)?;
        if overflowed {
            if value > 0 {
                "+bigint".to_owned()
            } else {
                "-bigint".to_owned()
            }
        } else {
            format_int(value)
        }
    } else if flags & PY_TPFLAGS_STRING_SUBCLASS != 0
        || (version.major == 2 && (flags & PY_TPFLAGS_BYTES_SUBCLASS != 0))
    {
        let value = copy_string(addr as *const I::StringObject, process, false)?
            .replace('\'', "\\\"")
            .replace('\n', "\\n");
        if let Some((offset, _)) = value.char_indices().nth((max_length - 5) as usize) {
            format!("\"{}...\"", &value[..offset])
        } else {
            format!("\"{value}\"")
        }
    } else if flags & PY_TPFLAGS_DICT_SUBCLASS != 0 {
        if version.major == 3 && version.minor >= 6 {
            let mut values = Vec::new();
            let mut remaining = max_length - 2;
            for entry in DictIterator::from(process, version, addr)? {
                let (key, value) = entry?;
                let key = format_variable::<I, P>(process, version, key, remaining)?;
                let value = format_variable::<I, P>(process, version, value, remaining)?;
                remaining -= (key.len() + value.len()) as isize + 4;
                if remaining <= 5 {
                    values.push("...".to_owned());
                    break;
                }
                values.push(format!("{key}: {value}"));
            }
            format!("{{{}}}", values.join(", "))
        } else {
            // TODO: support getting dictionaries from older versions of python
            "dict".to_owned()
        }
    } else if flags & PY_TPFLAGS_LIST_SUBCLASS != 0 {
        let object: I::ListObject = process.copy_struct(addr)?;
        let addr = object.item() as usize;
        let mut values = Vec::new();
        let mut remaining = max_length - 2;
        for i in 0..object.size() {
            let valueptr: *mut I::Object =
                process.copy_struct(addr + i * std::mem::size_of::<*mut I::Object>())?;
            let value = format_variable::<I, P>(process, version, valueptr as usize, remaining)?;
            remaining -= value.len() as isize + 2;
            if remaining <= 5 {
                values.push("...".to_owned());
                break;
            }
            values.push(value);
        }
        format!("[{}]", values.join(", "))
    } else if flags & PY_TPFLAGS_TUPLE_SUBCLASS != 0 {
        let object: I::TupleObject = process.copy_struct(addr)?;
        let mut values = Vec::new();
        let mut remaining = max_length - 2;
        for i in 0..object.size() {
            let value_addr: *mut I::Object = process.copy_struct(object.address(addr, i))?;
            let value = format_variable::<I, P>(process, version, value_addr as usize, remaining)?;
            remaining -= value.len() as isize + 2;
            if remaining <= 5 {
                values.push("...".to_owned());
                break;
            }
            values.push(value);
        }
        format!("({})", values.join(", "))
    } else if value_type_name == "float" {
        let value =
            process.copy_pointer(addr as *const crate::python_bindings::v3_7_0::PyFloatObject)?;
        format!("{}", value.ob_fval)
    } else if value_type_name == "NoneType" {
        "None".to_owned()
    } else if value_type_name.starts_with("numpy.") {
        match value_type_name {
            // u8, not bool: any byte value is possible, and bool would be UB
            "numpy.bool" => format_obval::<u8, P>(addr, process)?,
            "numpy.uint8" => format_obval::<u8, P>(addr, process)?,
            "numpy.uint16" => format_obval::<u16, P>(addr, process)?,
            "numpy.uint32" => format_obval::<u32, P>(addr, process)?,
            "numpy.uint64" => format_obval::<u64, P>(addr, process)?,
            "numpy.int8" => format_obval::<i8, P>(addr, process)?,
            "numpy.int16" => format_obval::<i16, P>(addr, process)?,
            "numpy.int32" => format_obval::<i32, P>(addr, process)?,
            "numpy.int64" => format_obval::<i64, P>(addr, process)?,
            "numpy.float32" => format_obval::<f32, P>(addr, process)?,
            "numpy.float64" => format_obval::<f64, P>(addr, process)?,
            _ => format!("<{value_type_name} at 0x{addr:x}>"),
        }
    } else {
        format!("<{value_type_name} at 0x{addr:x}>")
    };

    Ok(formatted)
}

/// Format the numpy scalar to a string.
///
/// All numpy scalars have shape:
/// {
///     ob_base: PyObject,
///     obval: <value>,
/// }
///
/// Where `obval` can be of different sizes depending on the scalar type.
/// We match the size to the value_type_name for this purpose, avoiding the
/// need to build bindings for the numpy C API.
///
/// * `addr`: Address of the numpy scalar
/// * `process`: Process memory in which the object resides
fn format_obval<T, P>(addr: usize, process: &P) -> Result<String, Error>
where
    T: std::fmt::Display + Copy,
    P: ProcessMemory,
{
    let base_addr = addr as *mut u32;
    let offset = std::mem::size_of::<crate::python_bindings::v3_7_0::PyObject>() as isize;
    let result = unsafe { process.copy_pointer(base_addr.byte_offset(offset) as *const T)? };
    Ok(format!("{result}"))
}

#[cfg(test)]
pub mod tests {
    // the idea here is to create various cpython interpretator structs locally
    // and then test out that the above code handles appropriately
    use super::*;
    use crate::python_bindings::v3_7_0::{
        PyASCIIObject, PyBytesObject, PyUnicodeObject, PyVarObject,
    };
    use remoteprocess::LocalProcess;
    use std::ptr::copy_nonoverlapping;

    // python stores data after pybytesobject/pyasciiobject. hack by initializing a 4k buffer for testing.
    // TODO: get better at Rust and figure out a better solution
    #[allow(dead_code)]
    #[repr(C)]
    pub struct AllocatedPyByteObject {
        pub base: PyBytesObject,
        pub storage: [u8; 4096],
    }

    #[allow(dead_code)]
    #[repr(C)] // Rust can optimize the layout of this struct and break our pointer arithmetic
    pub struct AllocatedPyASCIIObject {
        pub base: PyASCIIObject,
        pub storage: [u8; 4096],
    }

    pub fn to_byteobject(bytes: &[u8]) -> AllocatedPyByteObject {
        let ob_size = bytes.len() as isize;
        let base = PyBytesObject {
            ob_base: PyVarObject {
                ob_size,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut ret = AllocatedPyByteObject {
            base,
            storage: [0 as u8; 4096],
        };
        unsafe {
            copy_nonoverlapping(
                bytes.as_ptr(),
                ret.base.ob_sval.as_mut_ptr() as *mut u8,
                bytes.len(),
            );
        }
        ret
    }

    pub fn to_asciiobject(input: &str) -> AllocatedPyASCIIObject {
        let bytes: Vec<u8> = input.bytes().collect();
        let mut base = PyASCIIObject {
            length: bytes.len() as isize,
            ..Default::default()
        };
        base.state.set_compact(1);
        base.state.set_kind(1);
        base.state.set_ascii(1);
        let mut ret = AllocatedPyASCIIObject {
            base,
            storage: [0 as u8; 4096],
        };
        unsafe {
            let ptr = &mut ret as *mut AllocatedPyASCIIObject as *mut u8;
            let dst = ptr.offset(std::mem::size_of::<PyASCIIObject>() as isize);
            copy_nonoverlapping(bytes.as_ptr(), dst, bytes.len());
        }
        ret
    }

    /// kind=4 PyUnicodeObject built from raw code points, so tests can pass invalid ones
    pub fn to_ucs4object(code_points: &[u32]) -> AllocatedPyASCIIObject {
        let mut base = PyASCIIObject {
            length: code_points.len() as isize,
            ..Default::default()
        };
        base.state.set_compact(1);
        base.state.set_kind(4);
        base.state.set_ascii(0);
        let mut ret = AllocatedPyASCIIObject {
            base,
            storage: [0 as u8; 4096],
        };
        unsafe {
            let ptr = &mut ret as *mut AllocatedPyASCIIObject as *mut u8;
            // a non-ascii compact object stores data after the PyCompactUnicodeObject header
            let dst = ptr.add(std::mem::size_of::<
                crate::python_bindings::v3_7_0::PyCompactUnicodeObject,
            >());
            for (i, cp) in code_points.iter().enumerate() {
                copy_nonoverlapping(cp.to_ne_bytes().as_ptr(), dst.add(i * 4), 4);
            }
        }
        ret
    }

    fn as_unicode(obj: &AllocatedPyASCIIObject) -> &PyUnicodeObject {
        unsafe { std::mem::transmute(&obj.base) }
    }

    #[test]
    fn test_copy_string() {
        let original = "function_name";
        let obj = to_asciiobject(original);

        let unicode: &PyUnicodeObject = unsafe { std::mem::transmute(&obj.base) };
        let copied = copy_string(unicode, &LocalProcess, false).unwrap();
        assert_eq!(copied, original);
    }

    #[test]
    fn test_copy_string_ucs4() {
        let original = "\u{1d4bd}\u{1d452}\u{1d4c1}\u{1d4c1}\u{1d45c}";
        let code_points: Vec<u32> = original.chars().map(|c| c as u32).collect();
        let obj = to_ucs4object(&code_points);

        assert_eq!(
            copy_string(as_unicode(&obj), &LocalProcess, false).unwrap(),
            original
        );
        assert_eq!(
            copy_string(as_unicode(&obj), &LocalProcess, true).unwrap(),
            original
        );
    }

    #[test]
    fn test_copy_string_ucs4_invalid_code_point() {
        // past the last code point, a surrogate, and garbage: none is a valid `char`
        for invalid in [0x0011_0000, 0x0000_d800, 0xffff_ffff] {
            let obj = to_ucs4object(&['a' as u32, invalid, 'b' as u32]);

            assert_eq!(
                copy_string(as_unicode(&obj), &LocalProcess, false).unwrap(),
                "a\u{fffd}b"
            );

            let err = copy_string(as_unicode(&obj), &LocalProcess, true).unwrap_err();
            assert!(is_invalid_string(&err), "unexpected error: {err:#}");
        }
    }

    #[test]
    fn test_copy_string_ascii_with_high_byte() {
        let mut obj = to_asciiobject("abc");
        unsafe {
            let ptr = &mut obj as *mut AllocatedPyASCIIObject as *mut u8;
            let dst = ptr.add(std::mem::size_of::<PyASCIIObject>());
            *dst.add(1) = 0xff;
        }

        let err = copy_string(as_unicode(&obj), &LocalProcess, true).unwrap_err();
        assert!(is_invalid_string(&err), "unexpected error: {err:#}");

        // lenient still rejects it, but not as an InvalidString: frame skipped, sample kept
        let err = copy_string(as_unicode(&obj), &LocalProcess, false).unwrap_err();
        assert!(!is_invalid_string(&err));
    }

    #[test]
    fn test_is_invalid_string_survives_context() {
        use anyhow::Context;

        // if this regressed to an outermost-only downcast, --check-utf8 would silently do
        // nothing, since every call site wraps the error
        let err: Error = InvalidString.into();
        assert!(is_invalid_string(&err));

        let wrapped = Err::<(), Error>(err)
            .context("Failed to copy filename")
            .unwrap_err();
        assert!(is_invalid_string(&wrapped));

        // and not the ordinary py3.13+ "wasn't a PyCodeObject" case, or we'd discard those too
        let other =
            format_err!("Failed to copy PyFrameObject").context("Failed to call get_stack_trace");
        assert!(!is_invalid_string(&other));
    }

    #[test]
    fn test_copy_bytes() {
        let original = [10_u8, 20, 30, 40, 50, 70, 80];
        let bytes = to_byteobject(&original);
        let copied = copy_bytes(&bytes.base, &LocalProcess).unwrap();
        assert_eq!(copied, original);
    }
}
