---
name: code-review
description: Review a change set for functional correctness, drift from the plan, over-engineering and unreasonable codebase growth, and conformance to modern Rust practice and AGENTS.md. Use when asked to review code, a branch, a diff, or a pull request in this repository.
whenToUse: The user asks for a code review, or a change set must be verified before merge, release, or handoff.
---

# Code Review

Review the change set as an independent reviewer, not as its author. Be
evidence-driven: every finding cites a file and line, and every claim about
tests or behavior is verified or marked as unverified.

## Establish Scope First

1. Read the request, the plan, and the design documents that govern the
   changed area (`docs/design/`).
2. Identify the exact diff: `git status`, `git diff <base>...HEAD`, the list
   of commits, and the PR description if present.
3. Read every changed file in full, plus the callers and tests that depend on
   it. Do not review from the diff alone when behavior crosses a boundary.
4. Note what cannot be reviewed (missing plan, missing context, platform-only
   code) instead of guessing.

## 1. Functional Correctness

- Trace each changed behavior end to end: inputs, state transitions, outputs,
  error paths, retries, cancellation, and shutdown.
- Check boundary and failure cases: empty or oversized input, invalid or
  missing values, time zone and clock assumptions, partial writes, concurrent
  access, disconnects, and duplicate delivery.
- Check invariants at every public boundary: validation happens before state
  changes, errors propagate instead of being swallowed, and no success is
  reported after a failure.
- Check resource lifecycle: spans, locks, tasks, file handles, and temporary
  files are released on both success and error paths.
- Verify tests exercise the changed behavior, including the failure cases;
  confirm assertions would fail if the implementation regressed.
- Run the smallest relevant checks (`cargo fmt --all --check`,
  `cargo clippy --workspace --all-targets --all-features -- -D warnings`,
  targeted `cargo test`) when feasible. Report the exact commands and results;
  never claim a check passed without running it.

## 2. Drift From the Plan

- Compare the diff against the stated goal, issue, or plan: what was promised,
  what was delivered, and what changed without being requested.
- Flag missing pieces (unimplemented requirements, skipped edge cases,
  incomplete migrations or docs) and unplanned additions (extra features,
  unrelated refactors, renamed public contracts, changed error codes or wire
  formats).
- Check that contract changes update the corresponding design documents in the
  same change set.
- If no plan exists, review against the request and the commit messages, and
  say explicitly that plan alignment could not be fully assessed.

## 3. Over-Engineering and Codebase Growth

- Challenge abstraction: traits with a single implementation and no test seam,
  generic parameters used once, configuration knobs nobody sets, extension
  points for hypothetical needs, and layers that only forward calls.
- Challenge duplication: the same logic in two places, parallel types that
  should be one, or a new module that repeats an existing responsibility.
- Check for dead code, commented-out code, speculative helpers, unused
  dependencies, and debug output.
- Judge size: compare files, lines, and dependencies added against the value
  delivered. A large diff for a small change, a new crate for one helper, or
  growth that outpaces the contract is a finding — quantify it.
- Prefer the simplest implementation that satisfies the contract; if a smaller
  design exists, describe it concretely.

## 4. Modern Rust Practice and AGENTS.md

- Idiomatic Rust: ownership and borrowing over cloning, enums and newtypes
  over loose primitives, `Result` over sentinel values, no needless `unsafe`,
  and APIs that make invalid states unrepresentable.
- Async discipline: no blocking work on the runtime, no lock held across
  `.await`, no detached task without an owner, and no cancellation or timeout
  behavior left implicit.
- Error handling: typed public errors, sources preserved, no `panic!`,
  `unwrap`, or `expect` outside tests, and no log-and-continue.
- Conformance to `AGENTS.md`, at minimum:
  - no backward-compatibility shims unless explicitly requested or documented
    as deployed;
  - the pinned third-party library choices and dependency rules;
  - module layout, public surface, and documentation rules;
  - deterministic tests and regression coverage for bug fixes;
  - commit hygiene (Conventional Commits, one logical change per commit, no
    unrelated reformatting) and a clean working tree.
- Do not invent style preferences beyond `rustfmt`, `clippy`, and `AGENTS.md`;
  label taste-level comments as non-blocking.

## Output Format

Report findings grouped by severity, each with:

- **Location** — `path:line`;
- **Evidence** — what the code does and why it is wrong or risky;
- **Impact** — user-visible, data, or maintenance consequence;
- **Fix** — the smallest correct change.

Then give:

1. A verdict: **approve**, **approve with nits**, or **request changes**.
2. Open questions or assumptions.
3. What was verified (commands run, tests read) and what was not.

Keep blocking findings separate from nits. Do not rewrite the code unless the
user asks for fixes; stay read-only during review. Review the change set, not
the whole repository.
