# Passwords in the system keychain

This document describes the first increment. Separate entries per source were later replaced by the [shared Keychain vault](keychain-vault.md), which removes the series of reads at startup.

Increment plan: a separate configuration worker, events through the backend, and a versioned Keychain entry for the three secrets of a source. The stable source UUID and the configuration UUID identify the owner. The new entry is written and read back for verification before the configuration is replaced atomically. The previous entry is deleted only after the replacement. A failed migration keeps the original file. Regular serialization and Debug never contain secrets.

- [x] Model, native macOS store, serialized worker and atomic writes.
- [x] Migration, safe errors and retries, Forever / Until restart.
- [x] Apply / OK confirm only after a successful write; Cancel and Test Connection do not write.
- [x] Tests for errors, restart, editing, deletion and isolation.
- [x] UI tests, screenshots of the normal and small window, measurements, documentation.
- [x] Independent verification and a local commit, without pushing.

Application tests explicitly inject an in-memory store. The native test uses only a dedicated temporary keychain. We do not run the application with the real configuration and do not connect to an external database.

## Verification results

Full `cargo test -- --test-threads=1`: 470 tests passed, 0 failures, 30 skipped by default. The native test on a dedicated temporary keychain was run separately: 1 test passed in 2.46 s. Independent reviews of the code and of failure cases were completed after fixes.

`cargo clippy --all-targets -- -D warnings` and `cargo build --release` passed. The release build took 48.27 s. `target/release/DBeast.app` was refreshed; the strict ad hoc signature check and plist validation passed. The application was not run with the real configuration.

Screenshots 80-81 were reviewed, including the 1100×500 window; they stay out of git. Release measurement for 100 sources with an in-memory store and real durable file writes: migration 1.045 s, average autosave 10.601 ms. The scope and the command are in the README; the measurement does not include native Keychain latency or UI frame time.
