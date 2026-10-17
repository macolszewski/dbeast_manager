# Contributing to DBeast

Thanks for your interest in DBeast. This guide covers the local setup and the
conventions every change is expected to follow. By participating you agree to
the [Code of Conduct](./CODE_OF_CONDUCT.md). Security problems go through
[SECURITY.md](./SECURITY.md), not the issue tracker.

## Development setup

You need a recent stable Rust toolchain (the crate uses edition 2024), Docker
for the integration tests and, on Linux, the system libraries listed in the
[README](./README.md#building-from-source).

```bash
./scripts/test-dbs.sh          # PostgreSQL :55432, PostGIS :55433, MySQL :53306, SSH :52222
cargo run                      # debug build
cargo build --release && ./target/release/dbeast
./scripts/bundle-macos.sh      # macOS: target/release/DBeast.app
```

Point `DBEAST_CONFIG` at a temporary file when you experiment, so your real
data sources and session stay untouched:

```bash
DBEAST_CONFIG=/tmp/dbeast-dev/config.json cargo run
```

## Before you open a pull request

```bash
cargo clippy --all-targets     # must be warning-free
cargo test                     # tests that need a database are skipped without containers
```

- **Formatting.** The code base is not formatted with `rustfmt`; do not run
  `cargo fmt` on the whole tree. Match the style of the surrounding code.
- **Tests.** Every user-visible change comes with a test: a unit test or a UI
  test in `tests/ui_*.rs` (egui_kittest with wgpu). UI tests use
  `App::new_for_test` and a temporary `DBEAST_CONFIG`; they never read the
  real configuration or the system keychain.
- **Screenshots.** UI tests write screenshots to `docs/screenshots/` (not
  committed). Look at the screenshot of every view you changed. When a view
  shown in the documentation changes, run `scripts/docs-screenshots.sh` to
  refresh the committed copies in `docs/images/`.
- **Documentation.** Describe user-visible behavior in the relevant page of
  the user guide (`docs/guide/`) and add new shortcuts to
  `docs/reference/keyboard-shortcuts.md`. Preview the site with
  `pip install -r docs/requirements.txt && mkdocs serve`.
- **Performance.** The UI thread only draws. Database, network and disk work
  goes through the backend runtime and returns as events. Do not add
  per-frame work proportional to the data size. When a change can affect
  startup time, memory or frame time, measure it and put the numbers in the
  pull request (see [Performance](docs/reference/performance.md)).
- **Secrets.** Never print, log or commit passwords, keys or tokens, also not
  in tests.

## Commits and pull requests

- Commit messages follow [Conventional Commits](https://www.conventionalcommits.org/),
  in English: `feat(console): add statement history search`.
- Keep pull requests focused. Describe what changed and how you verified it.
- Add a line to the `Unreleased` section of [CHANGELOG.md](./CHANGELOG.md)
  for user-visible changes.

## Project layout

The architecture overview, module map and coding rules are in
[AGENTS.md](./AGENTS.md); it is written for AI coding agents and humans alike.

## License

By contributing you agree that your contributions are licensed under the
[Apache License 2.0](./LICENSE).
