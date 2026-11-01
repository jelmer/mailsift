Scripts for the `run_one` tests in `src/extractor.rs`, which exercise
process handling (pipe pressure, stdin, stderr, exit codes) rather than
extraction itself.

They live here, committed with the executable bit set, rather than being
written into a tempdir by each test. `execve` fails with `ETXTBSY` if any
process holds the file open for writing, and under `cargo test`'s thread
pool a `fork` in one thread inherits the write fd another thread has open
between its own write and close. Writing these at test time made the
suite fail intermittently; checking them in removes the write entirely.

They have no accompanying `.yaml`, so they are deliberately not in
`tests/fixtures/extractors/` where `discover` would pick them up.
