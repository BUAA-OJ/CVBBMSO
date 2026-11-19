# AGENTS.md

Guidance for coding agents working on `cvbbmso`.

## 1. Project Overview

`cvbbmso` is a proctored-exam system for Windows and macOS: a Tauri 2 desktop
client for students and an Axum server operated by staff. Students use a
controlled WebView to log in to Accoding; the server re-verifies the
`connect.sid` cookie against Accoding. Exam timing is server-authoritative and
commands are delivered over one WebSocket per student. During the app lifetime,
a system-level egress guard restricts outbound network traffic to Accoding, the
monitoring server, and essential local services.

The repository is being scaffolded crate by crate. The design contracts live
under `docs/design/`; follow their layout and contracts exactly.

### Source of truth

Read these before changing behavior:

1. `docs/design/overview.md` — system boundary, terminology, crate graph.
2. `docs/design/crates/cvbbmso-protocol.md` — wire format and error codes.
3. The crate document for the crate you are touching.

The documents under `docs/design/` are written in Chinese and define the public
contracts. Code, comments, doc comments, and commit messages are written in
English.

### Crate layout

```text
crates/
├── cvbbmso-protocol/   # HTTP / WebSocket DTOs, enums, error codes
├── cvbbmso-accoding/   # Cookie normalization, Accoding identity verification
├── cvbbmso-network/    # Client startup preflight and system egress guard
├── cvbbmso-recorder/   # Screen capture, segmentation, recovery, merge, hashing
├── cvbbmso-exam/       # Client exam runtime, WebSocket, local event queue
├── cvbbmso-client/     # Tauri desktop app, startup preflight, controlled browser
├── cvbbmso-domain/     # Exam rules, scheduling, persistence, incidents
├── cvbbmso-server/     # Axum HTTP / WebSocket service
└── cvbbmso-cli/        # Local admin CLI for the server
```

Dependency direction is one-way and must not form cycles:

```text
client  -> exam -> recorder / accoding / protocol
client  -> network / accoding / protocol
server  -> domain / accoding / protocol
cli     -> domain / protocol
domain  -> protocol
recorder -> protocol
accoding -> protocol
```

`cvbbmso-protocol` is a pure data layer: no I/O, no async, no business logic.

## 2. Third-Party Library Preferences

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
| Database | `SeaORM` over SQLite (`sqlx` backend) | Domain crate only; entities stay private. |
| Migrations | `sea-orm-migration` | SeaORM's own migration framework; no `refinery`. |
| CLI parsing | `clap` (derive) | |
| Configuration | `clap` flags + environment variables | No configuration files for the server. |
| Logging | `tracing` + `tracing-subscriber` | JSON logs for the server, human-readable for the client. |
| Desktop shell | Tauri 2 | Controlled WebView only; no extra browser plugins. |
| CSV import | `csv` | CLI-side parsing only. |
| Test helpers | `proptest`, `wiremock`, `insta`, `tempfile` | See §6. |

Additional dependency rules:

- Declare every version once in `[workspace.dependencies]`; crates inherit with
  `workspace = true`.
- Prefer `default-features = false` and enable only the features actually used.
- Do not add dependencies from git, path patches, or forks unless unavoidable.
- Do not add a crate for something the standard library or an existing
  dependency already provides.
- Commit `Cargo.lock`. Keep one major version of each dependency.
- Run `cargo-deny check` and `cargo audit` before release.

## 3. Error Handling Rules

- Each library crate exposes exactly one public error enum. Variants are
  categories, not messages; add `#[non_exhaustive]`.
- Public APIs never return `anyhow::Error`, `Box<dyn Error>`, or `String` as an
  error. Convert to `anyhow` only at the binary boundary.
- Use `thiserror` and preserve sources with `#[source]` / `#[from]`. Do not
  discard the underlying error.
- Constructors validate their inputs and return `Result`; do not create values
  that violate documented invariants.
- Never use `panic!`, `unwrap`, or `expect` in non-test code. If an invariant
  is truly impossible to violate, encode it in the type instead.
- Do not convert an error into a log line and then return success. Log once,
  where the error is finally handled.
- Map errors to protocol error codes only at the transport boundary
  (`cvbbmso-server`) or CLI boundary. The mapping tables live in the crate
  docs; do not invent new codes.
- `DomainError` and similar enums must not carry HTTP status codes or
  user-facing copy. That mapping belongs to the caller.

## 4. Development Standards

Write idiomatic Rust and keep the design boring.

- **Abstraction must be earned.** Add a trait only when there is a second
  implementation, a real test seam, or a platform boundary. A trait with one
  implementation and no seam is noise.
- **High cohesion, low coupling.** A crate owns its data and hides its
  internals. Cross-crate access goes through the documented public API; no
  reaching into another crate's tables, files, or platform handles.
- **Simplest implementation that satisfies the contract.** Do not add
  configuration knobs, extension points, generic parameters, or layers for
  hypothetical future needs.
- **No duplicate ownership.** Each piece of state has one owner. Pass
  dependencies in through constructors; avoid global mutable state and lazy
  singletons.
- **Prefer enums and newtypes** for closed sets and identifiers. Do not pass
  raw `String`/`i64` where a validated type exists.
- **Keep public surface small.** Default to private; use `pub(crate)` before
  `pub`. Re-export only what other crates need.
- **Modules follow responsibility, not file count.** A module should have one
  reason to change. Split by responsibility, not by line count.
- **Use the sibling module layout.** A module with child modules lives in
  `module.rs` next to a `module/` directory (for example `src/wire.rs` plus
  `src/wire/`); never use the old `module/mod.rs` layout.
- **Comments explain why.** Do not narrate what the code already says. Public
  items get doc comments that state the contract and failure modes.
- **No dead code, no commented-out code, no debug prints.** Use `tracing`.
- **No blocking work in async context** without explicit justification; long
  CPU or filesystem work must not stall the runtime.
- **Never hold a lock across `.await`.** Prefer message passing over shared
  mutable state.
- **Do not over-abstract the database.** Use SeaORM entities and queries
  directly in `cvbbmso-domain`; a repository trait per table is not required.
- **Do not leak platform details.** Windows/macOS APIs stay inside
  `cvbbmso-recorder`, `cvbbmso-network`, and `cvbbmso-client` adapter modules.
- **Keep changes scoped.** Do not refactor unrelated code, rename public items,
  or reformat untouched files in the same change.

### Crate responsibilities

- `cvbbmso-protocol`: DTOs, enums, validation newtypes, error-code constants.
  No I/O, no async, no database types.
- `cvbbmso-accoding`: cookie handling and identity verification only.
- `cvbbmso-network`: client startup preflight and system egress guard.
  Proxy inspection stays read-only; never modify system proxy settings.
- `cvbbmso-recorder`: recording lifecycle, local files, manifests. Platform
  capture and encoding stay private.
- `cvbbmso-exam`: runtime orchestration, WebSocket, retry/reconnect, event
  queue, state publication. No network preflight and no Tauri types.
- `cvbbmso-client`: startup preflight, Tauri commands and UI wiring only. No
  exam business rules.
- `cvbbmso-domain`: rules, transactions, scheduling, persistence, audit.
  No HTTP or WebSocket types beyond protocol DTOs.
- `cvbbmso-server`: transport, authentication handoff, error mapping, dispatch.
  No business rules.
- `cvbbmso-cli`: argument parsing, CSV reading, output formatting. Delegates to
  `AdminService`.

## 5. Protocol and Data Conventions

- `cvbbmso-protocol` is the only source of wire formats. Do not redefine DTOs
  in other crates.
- JSON fields and enum values use `snake_case`; error codes use uppercase
  `SNAKE_CASE`.
- Wire enums are closed. Unknown values are a protocol error, not a silent
  fallback.
- Validate at construction and on deserialization: UUIDs, SHA-256 digests,
  positive IDs, non-zero recording policy values, `start_at <= end_at`.
- Times are UTC `OffsetDateTime`; render RFC 3339. Never store or transmit
  local time or naive timestamps.
- Database schema changes are forward-only `sea-orm-migration` migrations. The
  `Migrator` lives in a private module of `cvbbmso-domain`; keep each migration
  reversible and never edit an applied migration.
- Store timestamps in UTC. Filtering uses absolute instants.
- Deviations from the documented contracts require a docs change in the same
  change set.

## 6. Testing

- Unit tests live next to the code in `#[cfg(test)]` modules; integration
  tests live in `crates/<crate>/tests/`.
- Test behavior and contracts, not private implementation details.
- Use `proptest` for protocol round-trips and invariant checks.
- Use `insta` snapshots for serialized JSON so wire changes are visible.
- Use `wiremock` for Accoding HTTP interactions; do not hit the network.
- Use `tempfile` for recorder and filesystem tests.
- Prefer hand-written fakes for narrow traits; use `wiremock` at the HTTP
  boundary. Avoid `mockall`.
- Tests must be deterministic: no real sleeps, no wall-clock races, no
  ordering assumptions. Inject clocks and timeouts where behavior depends on
  time.
- Platform-specific recorder/network tests are gated by `cfg(target_os)` and
  skipped in CI when the platform is unavailable.
- A bug fix includes a regression test that fails before the fix.

## 7. Toolchain, Formatting, and Lints

- Edition 2024, MSRV 1.88, stable toolchain pinned in `rust-toolchain.toml`.
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

## 8. Git and CI

- Use Conventional Commits with English subjects, for example
  `feat(server): reject expired cookies` or `fix(recorder): keep segment index
  stable after restart`.
- One logical change per commit. Do not mix refactors with behavior changes.
- Keep the working tree free of generated artifacts except `Cargo.lock`.
- When CI exists, it must run:
  - `cargo fmt --all --check`
  - `cargo clippy --workspace --all-targets --all-features -- -D warnings`
  - `cargo test --workspace --all-features` on Linux
  - `cargo check --workspace --all-targets` for Windows and macOS targets
  - `cargo deny check` and `cargo audit`

## 9. Validation Commands

Run the smallest relevant checks after changes:

```sh
cargo fmt --all
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

These commands become available once the Cargo workspace is scaffolded. Until
then, documentation-only changes need no build.

## 10. Out of Scope for This File

Internal task orchestration, cancellation mechanics, retry algorithms, and the
recorder capture/encoding pipeline are implementation decisions. Do not infer
rules for them from this file; decide them in the crate and record the
reasoning in code comments or the crate document.

## 11. Work In Progress: Recorder Encoding

This section is a handoff note for the `cvbbmso-recorder` milestone. Delete it
once the milestone lands; it is not a standing rule of this repository.

State as of 2026-11-19, branch `feat/recorder-container`, no pull request yet.

- **M0–M2 complete.** Storage, recovery, the Matroska writer and reader, and the
  EBML and AVC primitives are implemented and unit tested with no new runtime
  dependency; the muxer is hand-written because no crate writes Matroska.
- **M3 complete.** `capture/windows.rs` duplicates the primary display and
  `capture::run` samples it at the recording frame rate on its own thread. The
  hardware tests are `#[ignore]`d because CI has no display; run
  `cargo test -p cvbbmso-recorder -- --ignored` on a real desktop, and note that
  the hook needs an escalated shell.
- **M4 complete.** `Encoder` negotiates H.264 output and NV12 input over Media
  Foundation, disables B-frames, allocates output samples, drains
  `MF_E_NOTACCEPTING` and stream changes, reads SPS/PPS from
  `MF_MT_MPEG_SEQUENCE_HEADER`, and returns four-byte length-prefixed
  `container::Sample`s plus an `AvcConfig` for `MkvWriter`. Its hardware test
  encodes real frames and parses the resulting Matroska file.
- **M6 complete.** `Recorder::finish` verifies every sealed segment, merges its
  samples into a hand-written MP4 (`final.mp4.tmp` then atomic rename), hashes
  the result, and caches the manifest. `RecordingError::Encoder` and
  `RecordingError::Merge` now carry an optional `std::io::Error` source.
- **M5 remains.** `Recorder::start` still returns `RecordingError::Platform`.
  The capture sampler is not yet connected to the encoder, segment rotation,
  `SegmentStore::seal`, `RecorderEvent::Fault`/`SegmentFinalized`, or the
  `stop`/retry lifecycle. Client integration should not treat `Recorder` as
  recording until that lands.

Environment notes that already cost time: `cargo test -p cvbbmso-recorder` works
now that `cvbbmso-protocol` declares the serde features it needs; the DSH sandbox
cannot reach crates.io or a display, so dependency downloads and the hardware
tests need an escalated shell.