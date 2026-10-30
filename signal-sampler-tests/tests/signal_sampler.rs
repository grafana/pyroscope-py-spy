//! Samples the CPython that pyo3 links, 3.11 to 3.14: build with e.g. `PYO3_PYTHON=python3.14`.
// macOS is left out: reading the embedding test binary's own libpython fails there with EINVAL.
#![cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]

use py_spy::config::{LockingStrategy, SignalTimer, TypesConfig};
use py_spy::sampler::Sampler;
use py_spy::Config;
use pyo3::prelude::*;
use pyo3::types::PyDict;

const SPIN: &std::ffi::CStr = c"
import time

def spin():
    end = time.monotonic() + 1.0
    while time.monotonic() < end:
        pass

spin()
";

#[test]
fn unwinds_in_signal_handler() {
    Python::attach(|py| {
        let version = py.version_info();
        assert_eq!(version.major, 3);
        assert!(
            (11..=14).contains(&version.minor),
            "unsupported {version:?}"
        );

        for signal_timer in [SignalTimer::ITimer, SignalTimer::CTimer] {
            check(py, signal_timer);
        }
    });
}

fn check(py: Python<'_>, signal_timer: SignalTimer) {
    {
        let config = Config {
            blocking: LockingStrategy::NonBlocking,
            unwind_in_signal_handler: true,
            type_checks: TypesConfig::Strict,
            signal_timer,
            include_thread_ids: true,
            sampling_rate: 239,
            ..Config::default()
        };
        let mut sampler = Sampler::new(std::process::id() as py_spy::Pid, &config).unwrap();

        let globals = PyDict::new(py);
        globals.set_item("__name__", "__main__").unwrap();

        // CTimer samples only the threads it was told about.
        if signal_timer == SignalTimer::CTimer {
            py.run(SPIN, Some(&globals), None).unwrap();
            let traces: usize = sampler
                .by_ref()
                .take(120)
                .map(|sample| sample.traces.len())
                .sum();
            assert_eq!(traces, 0);
            sampler
                .thread_created(py_spy::signal_sampler::timer::gettid())
                .unwrap();
        }
        py.run(SPIN, Some(&globals), None).unwrap();

        let mut samples = 0;
        let mut spins = 0;
        for sample in sampler.by_ref().take(150) {
            assert!(
                sample.sampling_errors.is_none(),
                "{:?}",
                sample.sampling_errors
            );
            for trace in sample.traces {
                samples += 1;
                if trace.frames.first().map(|f| f.name.as_str()) == Some("spin") {
                    spins += 1;
                    let names: Vec<_> = trace.frames.iter().map(|f| f.name.as_str()).collect();
                    assert_eq!(names, ["spin", "<module>"]);
                    assert!(trace.active);
                    assert_eq!(trace.frames[0].line, 0);
                }
            }
        }
        assert!(
            spins >= 50,
            "{signal_timer:?}: {spins} spin samples of {samples}"
        );
        sampler.thread_removed(py_spy::signal_sampler::timer::gettid());
    }
}
