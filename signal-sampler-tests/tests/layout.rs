use core::mem::offset_of;
use py_spy::signal_sampler::unwind::{Layout, OB_TYPE, V3_11, V3_12, V3_13, V3_14};

#[path = "../../src/python_bindings/v3_11_0.rs"]
mod v3_11_0;
#[path = "../../src/python_bindings/v3_12_0.rs"]
mod v3_12_0;
#[path = "../../src/python_bindings/v3_13_0.rs"]
mod v3_13_0;
#[path = "../../src/python_bindings/v3_14_0.rs"]
mod v3_14_0;

macro_rules! frame_checks {
    ($l:ty, $v:ident, $executable:ident, $owner:expr) => {{
        use crate::$v as v;
        assert_eq!(
            <$l>::EXECUTABLE,
            offset_of!(v::_PyInterpreterFrame, $executable)
        );
        assert_eq!(<$l>::PREVIOUS, offset_of!(v::_PyInterpreterFrame, previous));
        assert_eq!(<$l>::OWNER, $owner);
        assert_eq!(OB_TYPE, offset_of!(v::PyObject, ob_type));
    }};
}

#[test]
fn v3_11_matches_bindings() {
    use crate::v3_11_0 as v;
    frame_checks!(V3_11, v3_11_0, f_code, None);
    assert_eq!(V3_11::THREAD_FRAME, offset_of!(v::PyThreadState, cframe));
    assert_eq!(
        V3_11::CFRAME_FRAME,
        Some(offset_of!(v::_PyCFrame, current_frame))
    );
}

#[test]
fn v3_12_matches_bindings() {
    use crate::v3_12_0 as v;
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
    use crate::v3_13_0 as v;
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
    use crate::v3_14_0 as v;
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
