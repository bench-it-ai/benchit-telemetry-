# Contributing

* `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings` and
  `cargo test --workspace` must pass (CI runs them, plus an MSRV test on Rust 1.89, `cargo doc`
  and `cargo deny`).
* New collectors and parsers should be testable against a fake `/proc` / `/sys` tree; see the
  tests in `crates/benchit-telemetry-core/src/collect.rs`.
* Changes to the SQLite schema need a version bump (`SCHEMA_VERSION`), a migration, and a test
  that opens a database created by the previous version.
* The API is unauthenticated and local-only; changes that widen what it exposes need a note in
  `SECURITY.md`.
* Licensed under MIT OR Apache-2.0; contributions are accepted under the same terms.
