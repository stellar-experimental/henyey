# Repository Guidelines

This codebase is under heavy construction. Don't be afraid to make significant changes — refactors, redesigns, and large diffs are expected and welcome.

## Project Structure & Module Organization

- `crates/` contains the Rust workspace crates. Each module is a crate (e.g., `crates/ledger`, `crates/tx`, `crates/history`).
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
- Never fail silently. If assumptions are not met, error out — do not gracefully recover.
- Prefer using types provided by `rs-stellar-xdr` over creating new ones and adding many conversions.

## Determinism & Parity

- Parity has two tiers (see `docs/PARITY.md` for the full definition). The **observable / interop surface MUST match** stellar-core: ledger/bucket hashes, transaction result & meta XDR, SCP/overlay wire bytes, history archive format, HTTP/RPC/CLI contracts, and crypto outputs. You **MAY deviate freely** on internal architecture, helper utilities, metrics, logging, admin/debug endpoints, and performance.
- Behavior on the observable surface must be deterministic; do not introduce new *observable* semantics. Internal redesigns are welcome — compare against stellar-core test vectors and edge cases for the observable surface.
- For protocol or consensus behavior, consult `stellar-core/` to mirror upstream decisions and sequencing.
- Update the relevant crate's `PARITY_STATUS.md` and the parity column in the main `README.md` Crate Overview when implementing or removing functionality that affects stellar-core parity.

## Testing Guidelines

- Use Rust's built-in test framework (`#[test]`).
- Unit tests go in the same module; integration tests go in `crates/<crate>/tests/`.
- Name tests by behavior, e.g., `test_execute_transaction_min_seq_num_precondition`.
- Run focused tests when possible to speed iteration, then run `cargo test --all` before submitting.
- No fuzz testing is required for this project.
- No baseline transaction meta testing is required.
- **Invariant testing is out of scope** — do not write unit tests for the `henyey-invariant` crate. Invariants are verified at runtime during ledger close; correctness is validated by the online/offline integration tests.
- Perform testing on testnet, not mainnet.
- **Bug investigation workflow**: When investigating a bug, always start by writing a narrow unit test that reproduces the bug and fails. Then fix the code until the test passes. Do not skip the failing-test-first step.
- **Unit test coverage**: When writing new code, ensure it is thoroughly covered by unit tests. Every public function and significant code path should have corresponding tests.
- **Henyey online & offline tests**: When asked to run the online (watcher) or offline (verify-execution) henyey tests, always run them in the background. Log output to a file and share the `tail -f` command with the user so they can follow along. For example:
  ```bash
  # Offline verification (background)
  nohup ./target/release/henyey offline verify-execution --testnet --from <START> --to <END> --stop-on-error --show-diff > ~/data/<session>/offline-verify.log 2>&1 &
  echo "Follow along: tail -f ~/data/<session>/offline-verify.log"

  # Online watcher (background)
  nohup ./target/release/henyey run --watcher --config configs/watcher-testnet.toml > ~/data/<session>/watcher.log 2>&1 &
  echo "Follow along: tail -f ~/data/<session>/watcher.log"
  ```

## Commit & Pull Request Guidelines

- Commit messages are short, imperative, and sentence case (examples: "Implement disk-backed bucket storage", "Optimize memory usage").
- **AI Agent Co-authorship**: Any commit that was authored or co-authored by an AI agent MUST include that agent (the tool/interface, not the underlying model) as a co-author using a Git trailer. Use the appropriate trailer for the agent involved:
  - GitHub Copilot (CLI or IDE): `Co-authored-by: GitHub Copilot <copilot@github.com>`
  - Codex: `Co-authored-by: Codex <codex@openai.com>`
  - Other agents: Use the agent/tool name, not the model name
- **Documentation updates**: Before committing new code, ensure the affected crate's README documentation and `PARITY_STATUS.md` are updated to reflect the changes.
- **Bug fixes**: As soon as a bug is fixed and its regression test passes, commit and push immediately. Do not wait to be told.
- **Fix review**: After committing a fix, review and consider: Is it true to the design of the system? Can there be similar issues? Can we redesign the system to avoid these category of issues.
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
- The stellar-core upstream is available as a git submodule at `stellar-core/` (pinned to v26.0.1) for parity checks.
