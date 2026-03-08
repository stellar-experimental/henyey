# Repository Guidelines

## Project Structure & Module Organization

- `crates/` contains the Rust workspace crates. Each module is a crate (e.g., `crates/ledger/` → crate name `henyey-ledger`, `crates/tx/` → `henyey-tx`, `crates/history/` → `henyey-history`).
- `crates/henyey/` is the main binary crate; other crates are libraries.
- Tests live alongside code in `crates/*/src` and in `crates/*/tests` for integration tests.
- Docs live in crate `README.md` files plus top-level `README.md` and `stellar-specs/` (git submodule).
- Config examples are in `configs/` and `*.toml` at the repo root.

## Build, Test, and Development Commands

- `cargo build --all` — build the entire workspace.
- `cargo test --all` — run all unit and integration tests.
- `cargo test -p henyey-ledger --tests` — run a focused crate’s integration tests.
- `cargo clippy --all` — run lint checks (recommended before PRs).

## Coding Style & Naming Conventions

- Follow standard Rust style (4-space indentation, snake_case for functions and modules, CamelCase for types).
- Keep modules small and focused; prefer adding logic inside the relevant crate instead of cross-crate helpers.
- Use descriptive error messages and map to XDR result codes where applicable.
- Fix cargo compiler warnings before submitting changes; keep the workspace warning-free where practical.

## Determinism & Parity

- Any observable behavior must be deterministic and identical to stellar-core (v25.x / p25).
- Align behavior by comparing against stellar-core test vectors and edge cases; do not introduce new semantics.
- For protocol or consensus behavior, consult `stellar-core/` to mirror upstream decisions and sequencing.
- Update the relevant crate's `PARITY_STATUS.md` and the parity column in the main `README.md` Crate Overview when implementing or removing functionality that affects stellar-core parity.

## Testing Guidelines

- Use Rust's built-in test framework (`#[test]`).
- Unit tests go in the same module; integration tests go in `crates/<crate>/tests/`.
- Name tests by behavior, e.g., `test_execute_transaction_min_seq_num_precondition`.
- Run focused tests when possible to speed iteration, then run `cargo test --all` before submitting.
- No fuzz testing is required for this project.
- No baseline transaction meta testing is required.
- Run unit and integration tests against testnet fixtures, not mainnet. Running a live validator node on mainnet is permitted for production operation (see `configs/validator-mainnet.toml`).
- **Bug investigation workflow**: When investigating a bug, always start by writing a narrow unit test that reproduces the bug and fails. Then fix the code until the test passes. Do not skip the failing-test-first step.
- **Hash mismatch debugging**: If you encounter a hash mismatch in an online node, attempt to recreate it in a targeted offline verify execution test.
- **Unit test coverage**: When writing new code, ensure it is thoroughly covered by unit tests. Every public function and significant code path should have corresponding tests.

## Commit & Pull Request Guidelines

- Commit messages are short, imperative, and sentence case (examples: "Implement disk-backed bucket storage", "Optimize memory usage").
- **AI Agent Co-authorship**: Any commit that was authored or co-authored by an AI agent MUST include that agent (the tool/interface, not the underlying model) as a co-author using a Git trailer. Use the appropriate trailer for the agent involved:
  - Claude Code: `Co-authored-by: Claude Code <claude-code@anthropic.com>`
  - GitHub Copilot (CLI or IDE): `Co-authored-by: GitHub Copilot <copilot@github.com>`
  - Codex: `Co-authored-by: Codex <codex@openai.com>`
  - Other agents: Use the agent/tool name, not the model name (e.g. not "Claude Sonnet 4.6")
- **Bug fixes**: As soon as a bug is fixed and its regression test passes, commit and push immediately. Do not wait to be told.
- **Other changes**: Commit and push when explicitly directed by the user.
- PRs should include: a clear description, the tests run, and documentation updates when behavior changes.
- Link related issues or stellar-core references where relevant.
- When possible, fixes should be committed alongside regression tests that would have caught the bug.
- If a push is rejected, pull with rebase and retry the push.

## Agent Communication

- Format assistant messages in a Claude-like style: concise, direct, and structured with short sections or bullets when helpful.

## Storage & Disk Usage

- Use `~/data` for all artifacts: build cache, data files, build artifacts, and any other generated output. Do not use the local filesystem outside the repo for these purposes to avoid running out of space.
- `~/data` is a shared volume used by multiple developers and agents. Namespace your data under `~/data/<agent-session-id>/` to avoid collisions, since multiple agents may run concurrently under the same user (e.g., `~/data/a1b2c3/cargo-target/`).

## Configuration & Operational Notes

- SQLite is the only supported database backend.
- Protocol support is 24+ only; do not add legacy protocol behavior.
- The stellar-core v25 upstream is available as a git submodule at `stellar-core/` (pinned to v25.0.1) for parity checks.
