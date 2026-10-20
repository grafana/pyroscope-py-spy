# AGENTS.md

## What this repo is

`grafana/pyroscope-py-spy` is a fork of [benfred/py-spy](https://github.com/benfred/py-spy).
Its only consumer is [grafana/pyroscope-python](https://github.com/grafana/pyroscope-python),
which depends on it as a Rust library, pinned by git `rev`, with `default-features = false`.
`remoteprocess` comes from the `grafana/pyroscope-remoteprocess` fork (see `Cargo.toml`).

Nobody ships the `py-spy` binary, the Python wheel, or any of the CLI output formats from
this fork. Judge changes, bugs, and issues by their effect on the library path below.

## Relationship with upstream

Upstream is not actively maintained, though it still gets occasional fixes. The fork is
permanent: we do not plan to contribute fixes upstream or to drop the fork.

- When we hit a bug upstream has not fixed, we fix it here.
- When upstream lands a fix or feature we need, we merge upstream into the fork.
- Keep our patches to the minimum pyroscope-python needs. Keep the diff between the fork
  and upstream small, so merging upstream stays easy.

## How pyroscope-python uses it

See `rust/src/lib.rs` and `rust/src/pyspy_backend.rs` in pyroscope-python.

- `py_spy::sampler::Sampler::new(pid, &config)`, iterated on a background thread.
- In-process: `pid` is the profiler's own process.
- `Config`:
  - `blocking: LockingStrategy::NonBlocking`
  - `native: false`
  - `subprocesses: false`
  - `include_thread_ids: true`
  - `duration: RecordDuration::Unlimited`
  - `sampling_rate`, `gil_only`, `lineno`, `include_idle` set from user options
  - everything else is `Config::default()`
- `StackTrace` fields read: `pid`, `thread_id`, `thread_name`, `active`, `owns_gil`, `frames`.
- `Frame` fields read: `name`, `filename`, `module`, `line`. Downstream tests construct
  `Frame` with a struct literal, so adding or removing a `Frame` field breaks them.
- Platforms: Linux amd64/arm64 (glibc and musl), macOS x86_64/aarch64. No Windows.
- Python 3.10 to 3.14.

Sampling is nonblocking and in-process, so the interpreter keeps running while we read
its memory. Torn reads, freed frames, and half-initialized objects are the normal case.

## Not used downstream

These stay in the tree because we track upstream. Keep them compiling, but do not
design around them, optimize them, or file issues whose impact is limited to them.

- `cli` feature: `src/main.rs`, `src/console_viewer.rs`, `src/flamegraph.rs`,
  `src/speedscope.rs`, `src/chrometrace.rs`, `src/dump.rs`, `src/coredump.rs`, clap
  argument parsing in `src/config.rs`.
- `unwind` feature (`--native`): `src/native_stack_trace.rs`, `src/cython.rs`, and the
  Python/native stack merge. `Frame::is_shim_entry` only matters for that merge.
- Local variable dumping: `dump_locals`, `Frame::locals`, value formatting in
  `src/python_data_access.rs` beyond strings.
- Subprocess sampling (`subprocesses`, `Sampler::new_subprocess_sampler`).
- `LockingStrategy::Lock` and `AlreadyLocked`.
- `include_idle` inside this crate (downstream filters on `StackTrace::active` itself).
- Windows, FreeBSD, and Python bindings older than 3.10.
- `examples/`, `ci/`, and the maturin/PyPI wheel jobs in `.github/workflows/build.yml`.

## Changing the public API

Anything in "How pyroscope-python uses it" is a contract. Changing it needs a matching
pyroscope-python change. Downstream picks up fixes by bumping the `rev` in its
`rust/Cargo.toml` after a merge here.

## Build and test

- `cargo build --lib --no-default-features` matches the downstream build.
- `cargo test --release` runs the full suite, as CI does.
