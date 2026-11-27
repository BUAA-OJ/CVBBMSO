# AGENTS.md

Guidance for coding agents working on `cvbbmso`.

This file records general engineering conventions. Project-specific contracts
live under `docs/design/`; read the overview, the protocol document, and the
document for the crate you are touching before changing behavior. Those
documents are written in Chinese. Code, comments, doc comments, and commit
messages are written in English.

Keep this file general apart from the pinned library choices below.
Implementation decisions belong in the crate document and in code comments,
not here.

## Compatibility Policy

Do not add backward-compatibility logic for an old feature unless the user
explicitly requests it, or the feature is explicitly documented as already
deployed. Only the user or the project docs can establish that a feature is
deployed; existing code, tests, or habits are not evidence.

Compatibility logic includes, but is not limited to:

- deprecated aliases, re-exports, or wrappers kept for old callers;
- readers or parsers that accept legacy data or wire formats alongside the
  current one;
- version fallbacks, migration shims, and feature flags that preserve the old
  behavior;
- defensive branches whose only purpose is to keep an obsolete path working.

Change the behavior, remove the old path, and update every caller, test, and
document in the same change. If removing a path would require a data migration
for users already in production, treat that as a deployment decision and
confirm it with the user first.

## Third-Party Library Preferences

Use the following choices. Do not introduce an alternative without updating
this file and explaining why.

| Concern | Choice | Notes |
| --- | --- | --- |
| Async runtime | `tokio` | The only async runtime in the workspace. |
| Async traits | Native `async fn` in traits | Use `async-trait` only when a trait must be `dyn`-compatible. |
| Library errors | `thiserror` | One public error enum per library crate. |
| Binary errors | `anyhow` | Only in `cvbbmso-server`, `cvbbmso-client`, `cvbbmso-cli` glue code. |
| Serialization | `serde` + `serde_json` | `cvbbmso-protocol` owns the wire schema. |
| Time | `time` | Use `OffsetDateTime`; store and transmit UTC RFC 3339. |
| IDs | `uuid` | Standard textual form on the wire. |
| Hashing | `sha2` (RustCrypto) | Lowercase 64-char hex digests. |
| HTTP client | `reqwest` with `rustls-tls` | Do not enable `native-tls` or system OpenSSL. |
| HTTP server | `axum` | `tower` / `tower-http` only where they replace custom middleware. |
| WebSocket client | `tokio-tungstenite` | |
| WebSocket server | `axum` built-in WebSocket extractor | |
| Database | `SeaORM` over SQLite (`sqlx` backend) | `cvbbmso-domain` owns business queries and keeps its entities private. |
| Migrations | `sea-orm-migration` | Prefer the default `sea-orm-cli` layout described in the domain crate document. |
| CLI parsing | `clap` (derive) | |
| Configuration | `clap` flags + environment variables | No configuration files for the server. |
| Logging | `tracing` + `tracing-subscriber` | Human-readable logs for the server and client. |
| Desktop shell | Tauri 2 | Controlled WebView only; no extra browser plugins. |
| CSV import | `csv` | CLI-side parsing only. |
| Test helpers | `proptest`, `wiremock`, `insta`, `tempfile` | See Testing. |

Additional dependency rules:

- Declare every version once in `[workspace.dependencies]`; crates inherit with
  `workspace = true`.
- Prefer `default-features = false` and enable only the features actually used.
- Do not add dependencies from git, path patches, or forks unless unavoidable.
- Do not add a crate for something the standard library or an existing
  dependency already provides.
- Commit `Cargo.lock`. Keep one major version of each dependency.
- Run `cargo-deny check` and `cargo audit` before release.

## Development Standards

Write idiomatic Rust and keep the design boring.

- **Abstraction must be earned.** Add a trait only when there is a second
  implementation, a real test seam, or a platform boundary. A trait with one
  implementation and no seam is noise.
- **High cohesion, low coupling.** A module owns its data and hides its
  internals. Other modules use the documented public API; no reaching into
  another module's tables, files, or platform handles.
- **Simplest implementation that satisfies the contract.** Do not add
  configuration knobs, extension points, generic parameters, or layers for
  hypothetical future needs.
- **No duplicate ownership.** Each piece of state has one owner. Pass
  dependencies in through constructors; avoid global mutable state and lazy
  singletons.
- **Prefer enums and newtypes** for closed sets and identifiers. Do not pass
  raw `String`/`i64` where a validated type exists.
- **Keep the public surface small.** Default to private; use `pub(crate)`
  before `pub`. Re-export only what other modules need.
- **Modules follow responsibility, not file count.** A module should have one
  reason to change. Split by responsibility, not by line count.
- **Use the sibling module layout.** A module with child modules lives in
  `module.rs` next to a `module/` directory (for example `src/wire.rs` plus
  `src/wire/`); never use the old `module/mod.rs` layout.
- **Comments explain why.** Do not narrate what the code already says. Public
  items get doc comments that state the contract and failure modes.
- **No dead code, no commented-out code, no debug prints.** Use the logging
  facility instead.
- **No blocking work in async context** without explicit justification; long
  CPU or filesystem work must not stall the runtime.
- **Never hold a lock across `.await`.** Prefer message passing over shared
  mutable state.
- **Keep changes scoped.** Do not refactor unrelated code, rename public items,
  or reformat untouched files in the same change.

## Error Handling

- Public APIs return typed errors, never strings or boxed trait objects.
  Convert to a dynamic error only at the binary boundary.
- Preserve the underlying cause when wrapping an error; do not discard it.
- Constructors validate their inputs and return `Result`; do not create values
  that violate documented invariants.
- Never use `panic!`, `unwrap`, or `expect` outside tests. If an invariant is
  truly impossible to violate, encode it in the type instead.
- Do not convert an error into a log line and then return success. Log once,
  where the error is finally handled.
- Error types describe what failed, not how it is presented. They must not
  carry transport status codes or user-facing copy; that mapping belongs to
  the caller.

## Testing

- Unit tests live next to the code in `#[cfg(test)]` modules; integration
  tests live in a dedicated tests directory of the package.
- Test behavior and contracts, not private implementation details.
- Tests must be deterministic: no real sleeps, no wall-clock races, no
  ordering assumptions. Inject clocks and timeouts where behavior depends on
  time.
- Do not hit the real network in tests. Use fakes or a local stub server.
- Prefer hand-written fakes for narrow traits; avoid general-purpose mocking
  frameworks.
- Platform-specific tests are gated by `cfg(target_os)` and skipped where the
  platform is unavailable.
- A bug fix includes a regression test that fails before the fix.

## Toolchain, Formatting, and Lints

- Keep the Rust toolchain pinned in `rust-toolchain.toml`; local builds and CI
  use the same release. Update `Cargo.toml` and the CI workflow together when
  raising it.
- Format with default `rustfmt` settings: `cargo fmt --all`.
- Enable `clippy::all` and `clippy::pedantic` at workspace level. Treat
  warnings as errors in CI, not during local iteration.
- Deny `unsafe_op_in_unsafe_fn`. `unsafe` is allowed only in platform/FFI
  modules, and every block needs a `// SAFETY:` comment explaining the
  invariant. Expose safe wrappers to the rest of the crate.
- Library crates enable `#![warn(missing_docs)]`. Public enums that may grow
  are `#[non_exhaustive]`.
- Public items must be documented, including panics, errors, and blocking
  behavior.

## Git

- Use Conventional Commits with English subjects, for example
  `feat(server): reject expired cookies` or
  `fix(recorder): keep segment index stable after restart`.
- One logical change per commit. Do not mix refactors with behavior changes.
- Keep the working tree free of generated artifacts except the lockfile.
- CI must enforce formatting, lints, tests, cross-platform compilation checks,
  and dependency auditing.

## Validation Commands

Run the smallest relevant checks after changes:

```sh
cargo fmt --all
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

Documentation-only changes need no build.
