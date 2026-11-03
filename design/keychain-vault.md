# A single protected Keychain item for the configuration

## Cause and outcome

The previous format created a separate item for each source and a new item identity whenever passwords changed. Startup read these items one by one. After the application signature changed, macOS could require a separate approval for each item. This matches the symptom of many prompts with several databases.

In the new format a single `vault-v1` item in the service `ai.inprojects.dbeast.credentials.<configuration UUID>` holds revisions of the four secrets of every source: the database password, the SSH password, the SSH private key passphrase and the TLS identity. After migration, a single vault read at startup loads all saved passwords into memory. A regular autosave of history, filters and layout neither reads nor writes Keychain. Forever and Until restart keep working.

All operations stay on the dedicated persistence worker. The tests and this increment do not use the real `config.json`, the login keychain or external databases.

## Durability and authorization

An update uses `SecKeychainItem::set_password`, which calls `SecKeychainItemModifyAttributesAndData` with empty attributes. It keeps the existing item and its ACL; it neither deletes nor recreates it. A new item is created only after `errSecItemNotFound`. Access denial and other errors are returned to the application, with no fallback to a plaintext write.

Order of a password change:

1. Lock the configuration file and the vault UUID namespace across processes. Check that the configuration has not changed since the last read. Before the first Keychain change, durably write the UUID to a journal, even when `config.json` does not exist yet. A restart recovers the same vault after an ambiguous write or a failure of the first configuration write.
2. Prepare a map containing the previous and new revisions. Before writing it, compare a fresh vault read with the remembered version, so that changes from another instance are not overwritten.
3. Update the same Keychain item and read back the written content for verification.
4. Replace the configuration atomically and confirm the write of the file and the directory.
5. Only after a durable commit, remove obsolete revisions inside the item. Even an empty vault keeps the item identity.

A failure of the write, of the verification read or of the configuration write leaves the old references readable. Uncertainty after a rename and a failed directory fsync keeps both revisions. An ambiguous result of a native operation blocks the use of a stale in-memory copy; the message requires the settings to be reloaded. The `credential_vault_cleanup` marker and the `credential_cleanup` list make sure cleanup is retried by Apply. Startup and regular autosave do not retry protected deletions. A vault read, owner or version error stops all cleanup, including the removal of legacy separate entries. After a failed first configuration write, the recovered UUID makes it possible to find uncommitted revisions and remove them through Apply.

The locks check the owner and private permissions of the file and reject symbolic links. The lock file is not deleted after release, so that parallel processes do not lock different inodes. An additional hash of the canonical configuration location is stored inside the protected vault. A copy of `config.json` at a different path cannot use the same vault or delete its revisions. After moving the configuration, restore the original location. A separate configuration must have a new UUID and passwords entered again. This is a deliberate security limitation, also when using `DBEAST_CONFIG`.

After a write error, the Data Sources and Drivers window offers Reload settings and discard changes, with an explicit note that the draft is lost after a successful reload. While it runs, it shows a loading state. A failed read of the file or the passwords keeps the unsaved changes; a successful one replaces the draft, keeps the selected source by ID and invalidates old connections and metadata of changed or deleted sources. It does not open any connection automatically.

## Migration and the limits of macOS prompts

A missing `credential_vault_version` indicates the older format. DBeast reads its entries, writes the shared item, verifies it, commits the configuration and only then deletes the old entries. The old journal `config.json.credentials-pending.json` is still recognized. It contains only identifiers, never passwords. A failure to read the first old entry stops the remaining protected reads in that run, instead of triggering further prompts after a denial.

Migration may require several approvals once, because each existing item is still subject to its own macOS authorization. There is no safe way to read these passwords while bypassing the existing approvals. A failed deletion of old items remains pending until it is explicitly retried through Apply.

A single vault read does not guarantee that there will be no system prompts. The current bundle has an ad hoc signature. A new build or a different executable can be treated by macOS as a different code identity. A stable Developer ID signature remains a distribution task. We do not add Allow all applications, do not change global ACLs and do not disable Keychain protection. The number and content of system unlock dialogs depend on macOS and the user's settings.

## Verification

- `cargo test --lib persistence -- --test-threads=1`: 28 passed, 1 measurement skipped by default, 1.76 s. A counter showed exactly one protected read with 100 sources; 10 autosaves and an unchanged Apply added no vault reads, writes or deletions.
- The settings reload state test passed in 0.06 s: an error keeps the draft, a success replaces it, keeps the selection and disconnects changed and deleted sources.
- The tests cover a missing UUID in an old configuration, a failed first write of a new configuration, old entries and plaintext passwords, a first denial, a deletion denial without retries at startup, a corrupted vault, an ambiguous native write, a denial and a mismatch in the verification read, configuration and directory durability errors, parallel instances, a stale vault copy, copying the configuration, Until restart, renaming, duplicating and deleting a source.
- `cargo test --test keychain_native --test ui_credentials -- --include-ignored --test-threads=1`: the native test passed in 0.12 s, UI in 1.98 s. The native test created only a dedicated temporary keychain and a synthetic password; it confirmed the same `SecKeychainItem` identity after the update and a successful read after reopening the handle.
- `cargo test --test ui_credentials_reload -- --test-threads=1`: passed in 2.03 s. The real UI flow: a write conflict, a visible reload action, a failed read that keeps the draft, a successful read and Apply again. Screenshots 90/91 were reviewed, including the 1100×500 window; the reload action and the buttons fit in the view.
- Screenshots `80-password-storage.png` and `81-password-storage-small.png` (1100×500 window) were reviewed, outside git. The layout of Data Sources and Drivers, Save, Forever, Until restart, Apply and Cancel stays consistent with the rest of the interface.

Final release measurement for 100 sources and four synthetic secrets, after adding the durable UUID journal: migration 29.798 ms, reload 0.812 ms and exactly one vault read. One hundred autosaves took 8.453 ms per write on average and performed no additional vault reads, writes or deletions. Command: `cargo test --release --lib persistence::tests::benchmark_config_and_credential_persistence -- --ignored --nocapture`. The vault is in memory, but the file, locks, fsync and rename are real. The result does not include native Keychain latency, macOS prompts or UI frame time; it does not guarantee 60 fps.

The final independent verification from October 15, 2026 covered the whole regular test set: **551 passed, 35 explicitly skipped, zero skipped because of unavailable test databases**. `cargo test -- --test-threads=1 --show-output` was run, followed by the remaining targets with `--no-fail-fast` and the three fixed UI tests. Doctests passed, with no examples to run. All commands used a temporary `DBEAST_CONFIG` and `CARGO_INCREMENTAL=0`; database access was limited to local PostgreSQL, PostGIS, MySQL and SSH containers and SQLite files.

The first runs found three races in the UI tests: restoring an old result before the current session was saved, deleting the directory before cleanup after Apply finished, and reading the previous SQL before an asynchronous write completed. The tests now wait, respectively, for the expected result in `session.json`, for Apply to finish and for the exact SQL in `config.json`. The existing assertions were kept. The final `cargo test --no-fail-fast --test ui_console_edit --test ui_credentials --test ui_search -- --test-threads=1 --show-output` passed all three targets in 21.03 s including compilation. Application behavior was not changed for these tests.

- `cargo clippy --all-targets -- -D warnings`: clean, 10.08 s.
- `git diff --check` and rustfmt on the nine changed Rust files: clean. Earlier formatting differences in `src/ui/dialog.rs`, unrelated to the task, were kept; the new block is formatted.
- `./scripts/bundle-macos.sh`: successful build in 40.14 s. The `target/release/DBeast.app` bundle contains arm64, a valid plist and passed `codesign --verify --deep --strict`. The signature remains ad hoc; the application was not run with the user's configuration.
- Two independent security and quality reviews finished with no remaining blockers. They covered UUID durability before the first vault write, refusing to clean up a foreign or inaccessible vault, and recovering settings in the UI.

Command logs, timings and a SHA-256 manifest of 141 files were kept for the verification run. During the regression run only the formatting of `src/ui/mod.rs` and the described synchronization of three tests were changed; the manifests from before these changes were kept. The final manifest showed no further changes after the tests and clippy, and production code was unchanged during the final measurement and bundle build. Screenshots 90/91 were reviewed again. The earlier successful test in a dedicated temporary keychain remains valid: the native read and update code has not changed since it was run.

## References

- [Apple Code Signing Guide](https://developer.apple.com/library/archive/documentation/Security/Conceptual/CodeSigningGuide/AboutCS/AboutCS.html): Keychain tracks the application's designated requirement; a stable code identity is a separate issue from the number of items.
- [Apple: modifying the data of an existing item](https://developer.apple.com/documentation/security/seckeychainitemmodifyattributesanddata(_:_:_:_:)).
- [security-framework 3.7.0: macOS password handling](https://docs.rs/security-framework/3.7.0/security_framework/os/macos/passwords/index.html): the actual sources of the dependency in use were also checked; the `set_generic_password` convenience function is not used, because its fallback covers more errors than a missing item.
