# SSL/TLS for database connections

!!! note "Update from October 16, 2026"
    Added the Require / Verify CA / Full verification modes, a PEM client certificate with a separate key (PKCS#8, PKCS#1 RSA, SEC1 EC) converted in memory to PKCS#12, and Browse… buttons. The deviation described below (PKCS#12 only) no longer applies; the current behavior is described in the user documentation.

This increment covers PostgreSQL and MySQL, including connections over SSH. `Use SSL` means required encryption, a valid certificate chain and a matching database host name. There is no mode that accepts any certificate and no fallback to a plaintext connection.

- [x] PostgreSQL `Require` with SSL, `Disable` without SSL; cancellation uses the same prepared connector.
- [x] MySQL without switching from TCP to a local socket, including over an SSH tunnel; TLS settings are kept for KILL QUERY.
- [x] CA in PEM (including a bundle) or DER; optional PKCS#12 client identity with certificate and key.
- [x] PKCS#12 password in the existing versioned Keychain store or in memory only; reading older three-secret bundles.
- [x] The Test Connection result and the connection description show the encryption reported by the current server session. The test result is bound to the form revision and the request identifier. The whole test has a 15 s limit.
- [x] Local tests for trust, host name, missing SSL, mTLS, cancellation and SSH.
- [x] Screenshots of the normal and small window, measurements, an independent review and full verification.
- [x] Local commit on master, without pushing.

## Decisions

We keep the `use_tls` field in the configuration. The new `tls` field has default values, so older configurations and saved sessions still load. With SSL previously enabled, a connection to a server without TLS will now correctly report an error. File paths are public configuration; the archive password is excluded from regular serialization and Debug. Cancellation does not read the files again, so deleting or replacing them after the session is opened does not change its identity.

A deliberate simplification: the shared `Client identity` field accepts a `.p12/.pfx` archive instead of separate PEM certificate and key files. This format is supported by both native drivers in use. The archive password follows the Save setting from General. We do not add an OpenSSL converter to the application or external processes when connecting.

Files are read and validated in `spawn_blocking`, with a maximum of 8 MiB per file; directories and FIFOs are rejected. On Unix, opening does not block on a FIFO even if the path is swapped. The native PKCS#12 import for PostgreSQL runs in the same worker. In MySQL the import is an internal synchronous part of the asynchronous driver, off the UI thread; we do not create a second copy of the native identity. The connection stays cancellable, instead of detaching the whole connection process into a non-cancellable worker.

The CA is added to the system's trusted authorities. We do not modify the system trust store. Native TLS may use its own temporary keychain for the client identity; the tests use only generated certificates and synthetic passwords.

## References

- [tokio-postgres: SslMode](https://docs.rs/tokio-postgres/latest/tokio_postgres/config/enum.SslMode.html): the difference between Prefer, Require and Disable.
- [native-tls: Identity](https://docs.rs/native-tls/0.2.18/native_tls/struct.Identity.html): PKCS#12 contains the certificate and the key, with a password for decryption.
- mysql_async 0.37.1, local crate sources: `opts/native_tls_opts.rs` (PKCS#12), `io/tls/native_tls_io.rs` (system CAs and name validation), `conn/mod.rs` (rejection when CLIENT_SSL is missing) and `opts/mod.rs` (prefer_socket).

## Increment verification

Final unit tests: 400 passed, 23 skipped (5.59 s). The full independent regression run passed 476 tests with no failures; 34 tests are skipped by default. The four TLS tests that are skipped by default were run separately with `scripts/test-tls.sh`: all passed (10.54 s), and the independent verifier repeated them successfully. They cover PostgreSQL and MySQL, trusted and foreign CAs, a missing CA, PEM/DER/bundle, a wrong host name, missing TLS, TLS explicitly disabled, a required client certificate, a wrong PKCS#12 password and cancellation, both direct and over SSH, after the certificate files were deleted. UI test `ui_tls`: 1 passed, 6.28 s; screenshots 82-83 (1280×800 and 1100×500) were reviewed. The new fields have associated accessibility labels. The screenshots show password masking, the TLS settings and the Apply/Cancel/OK footer reachable after scrolling. The screenshots stay out of git.

Measurements of a local debug run through Docker: the median of opening a connection, reading the state and running SELECT 1 was 3.9 ms without TLS, 10.8 ms with TLS and 39.0 ms with mTLS for MySQL; for PostgreSQL, 79.2 / 82.5 / 132.6 ms respectively. Each variant was run five times. Spread and limitations are documented in the README. Cancellation with a deliberate 300 ms delay took 324.4 ms directly (PostgreSQL) and 367.7 ms (MySQL), and 377.1 / 341.3 ms over SSH. The measurement does not include the UI.

The independent verification passed the four TLS tests again in 6.88 s. `cargo clippy --all-targets -- -D warnings` finished with no warnings (22.77 s), and `cargo build --release` succeeded (59.47 s). `target/release/DBeast.app` was rebuilt; the plist check and strict codesign signature verification passed. Independent quality and bug reviews finished with no open comments. The application was not run against the real configuration. The increment was committed locally on master, without pushing.
