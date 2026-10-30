/// Field offsets of one CPython minor version, default (GIL) build, 64-bit.
pub trait Layout {
    /// Offset in `PyThreadState` of the current frame, or of `cframe` if [`Layout::CFRAME_FRAME`] is set.
    const THREAD_FRAME: usize;
    /// Offset in `_PyCFrame` of the current frame.
    const CFRAME_FRAME: Option<usize>;
    const EXECUTABLE: usize;
    const EXECUTABLE_TAG_BITS: usize;
    const PREVIOUS: usize;
    const OWNER: Option<usize>;
    /// Bit `1 << owner` is set for owners that are not Python frames.
    const SKIP_OWNERS: u8;
}

const FRAME_OWNED_BY_CSTACK_3_12: u8 = 3;
const FRAME_OWNED_BY_INTERPRETER: u8 = 3;
const FRAME_OWNED_BY_CSTACK: u8 = 4;

pub struct V3_11;

impl Layout for V3_11 {
    const THREAD_FRAME: usize = 56;
    const CFRAME_FRAME: Option<usize> = Some(8);
    const EXECUTABLE: usize = 32;
    const EXECUTABLE_TAG_BITS: usize = 0;
    const PREVIOUS: usize = 48;
    const OWNER: Option<usize> = None;
    const SKIP_OWNERS: u8 = 0;
}

pub struct V3_12;

impl Layout for V3_12 {
    const THREAD_FRAME: usize = 56;
    const CFRAME_FRAME: Option<usize> = Some(0);
    const EXECUTABLE: usize = 0;
    const EXECUTABLE_TAG_BITS: usize = 0;
    const PREVIOUS: usize = 8;
    const OWNER: Option<usize> = Some(70);
    const SKIP_OWNERS: u8 = 1 << FRAME_OWNED_BY_CSTACK_3_12;
}

pub struct V3_13;

impl Layout for V3_13 {
    const THREAD_FRAME: usize = 72;
    const CFRAME_FRAME: Option<usize> = None;
    const EXECUTABLE: usize = 0;
    const EXECUTABLE_TAG_BITS: usize = 0;
    const PREVIOUS: usize = 8;
    const OWNER: Option<usize> = Some(70);
    const SKIP_OWNERS: u8 = 1 << FRAME_OWNED_BY_CSTACK_3_12;
}

pub struct V3_14;

impl Layout for V3_14 {
    const THREAD_FRAME: usize = 72;
    const CFRAME_FRAME: Option<usize> = None;
    const EXECUTABLE: usize = 0;
    const EXECUTABLE_TAG_BITS: usize = 3;
    const PREVIOUS: usize = 8;
    const OWNER: Option<usize> = Some(74);
    const SKIP_OWNERS: u8 = (1 << FRAME_OWNED_BY_INTERPRETER) | (1 << FRAME_OWNED_BY_CSTACK);
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::offset_of;

    macro_rules! frame_checks {
        ($l:ty, $v:ident, $executable:ident, $owner:expr) => {{
            use crate::python_bindings::$v as v;
            assert_eq!(
                <$l>::EXECUTABLE,
                offset_of!(v::_PyInterpreterFrame, $executable)
            );
            assert_eq!(<$l>::PREVIOUS, offset_of!(v::_PyInterpreterFrame, previous));
            assert_eq!(<$l>::OWNER, $owner);
            assert_eq!(super::super::OB_TYPE, offset_of!(v::PyObject, ob_type));
        }};
    }

    #[test]
    fn v3_11_matches_bindings() {
        use crate::python_bindings::v3_11_0 as v;
        frame_checks!(V3_11, v3_11_0, f_code, None);
        assert_eq!(V3_11::THREAD_FRAME, offset_of!(v::PyThreadState, cframe));
        assert_eq!(
            V3_11::CFRAME_FRAME,
            Some(offset_of!(v::_PyCFrame, current_frame))
        );
    }

    #[test]
    fn v3_12_matches_bindings() {
        use crate::python_bindings::v3_12_0 as v;
        frame_checks!(
            V3_12,
            v3_12_0,
            f_code,
            Some(offset_of!(v::_PyInterpreterFrame, owner))
        );
        assert_eq!(V3_12::THREAD_FRAME, offset_of!(v::PyThreadState, cframe));
        assert_eq!(
            V3_12::CFRAME_FRAME,
            Some(offset_of!(v::_PyCFrame, current_frame))
        );
        assert_eq!(
            V3_12::SKIP_OWNERS,
            1 << v::_frameowner_FRAME_OWNED_BY_CSTACK
        );
    }

    #[test]
    fn v3_13_matches_bindings() {
        use crate::python_bindings::v3_13_0 as v;
        frame_checks!(
            V3_13,
            v3_13_0,
            f_executable,
            Some(offset_of!(v::_PyInterpreterFrame, owner))
        );
        assert_eq!(
            V3_13::THREAD_FRAME,
            offset_of!(v::PyThreadState, current_frame)
        );
        assert_eq!(V3_13::CFRAME_FRAME, None);
        assert_eq!(
            V3_13::SKIP_OWNERS,
            1 << v::_frameowner_FRAME_OWNED_BY_CSTACK
        );
    }

    #[test]
    fn v3_14_matches_bindings() {
        use crate::python_bindings::v3_14_0 as v;
        frame_checks!(
            V3_14,
            v3_14_0,
            f_executable,
            Some(offset_of!(v::_PyInterpreterFrame, owner))
        );
        assert_eq!(
            V3_14::THREAD_FRAME,
            offset_of!(v::PyThreadState, current_frame)
        );
        assert_eq!(V3_14::CFRAME_FRAME, None);
        assert_eq!(V3_14::EXECUTABLE_TAG_BITS, v::Py_TAG_BITS as usize);
        assert_eq!(
            V3_14::SKIP_OWNERS,
            (1 << v::_frameowner_FRAME_OWNED_BY_INTERPRETER)
                | (1 << v::_frameowner_FRAME_OWNED_BY_CSTACK)
        );
    }
}
