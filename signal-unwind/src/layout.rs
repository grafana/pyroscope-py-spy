/// Field offsets of one CPython minor version, default (GIL) build, 64-bit.
pub trait Layout {
    const THREAD_FRAME: usize;
    const EXECUTABLE: usize;
    const EXECUTABLE_TAG_BITS: usize;
    const PREVIOUS: usize;
    const INSTR_PTR: usize;
    const OWNER: usize;
    /// Bit `1 << owner` is set for owners that are not Python frames.
    const SKIP_OWNERS: u8;
    const CODE_ADAPTIVE: usize;
}

const FRAME_OWNED_BY_INTERPRETER: u8 = 3;
const FRAME_OWNED_BY_CSTACK: u8 = 4;

pub struct V3_14;

impl Layout for V3_14 {
    const THREAD_FRAME: usize = 72;
    const EXECUTABLE: usize = 0;
    const EXECUTABLE_TAG_BITS: usize = 3;
    const PREVIOUS: usize = 8;
    const INSTR_PTR: usize = 56;
    const OWNER: usize = 74;
    const SKIP_OWNERS: u8 = (1 << FRAME_OWNED_BY_INTERPRETER) | (1 << FRAME_OWNED_BY_CSTACK);
    const CODE_ADAPTIVE: usize = 208;
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::offset_of;

    macro_rules! check {
        ($l:ty, $v:ident, $executable:ident, $instr_ptr:ident) => {{
            use crate::$v as v;
            assert_eq!(
                <$l>::EXECUTABLE,
                offset_of!(v::_PyInterpreterFrame, $executable)
            );
            assert_eq!(<$l>::PREVIOUS, offset_of!(v::_PyInterpreterFrame, previous));
            assert_eq!(
                <$l>::INSTR_PTR,
                offset_of!(v::_PyInterpreterFrame, $instr_ptr)
            );
            assert_eq!(<$l>::OWNER, offset_of!(v::_PyInterpreterFrame, owner));
            assert_eq!(
                <$l>::CODE_ADAPTIVE,
                offset_of!(v::PyCodeObject, co_code_adaptive)
            );
            assert_eq!(crate::OB_TYPE, offset_of!(v::PyObject, ob_type));
        }};
    }

    #[test]
    fn v3_14_matches_bindings() {
        use crate::v3_14_0 as v;
        check!(V3_14, v3_14_0, f_executable, instr_ptr);
        assert_eq!(
            V3_14::THREAD_FRAME,
            offset_of!(v::PyThreadState, current_frame)
        );
        assert_eq!(V3_14::EXECUTABLE_TAG_BITS, v::Py_TAG_BITS as usize);
        assert_eq!(
            V3_14::SKIP_OWNERS,
            (1 << v::_frameowner_FRAME_OWNED_BY_INTERPRETER)
                | (1 << v::_frameowner_FRAME_OWNED_BY_CSTACK)
        );
    }
}
