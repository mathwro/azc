# AGENTS.md

## Product contract

`azc` is a small Rust executable for changing the active Azure CLI subscription. Preserve these invariants:

- `azc` with no query enters the TUI and accepts printable search input immediately. There is no normal/insert mode.
- Vim-style navigation uses `Alt+j` / `Alt+k`, leaving plain `j` and `k` available to search.
- The TUI renders before `az account list` completes; subscription loading remains off the terminal event thread.
- Interactive mode changes context only after `Enter`. `Esc` and `Ctrl+C` leave context unchanged.
- Direct mode (`azc <query>`) switches only when the best match is unique. Equal-ranked matches return an ambiguity error rather than guessing.
- Context changes use the selected subscription ID, not its display name.
- Azure CLI is executed directly with `std::process::Command`; keep query and subscription data out of shell command strings.
- Ordinary listing uses Azure CLI's local account data. `--refresh` is the explicit server-refresh path; do not add a second subscription cache.
- `azc update` reinstalls from the manifest repository with Cargo; `azc version` reports `CARGO_PKG_VERSION`. `azc -- update` and `azc -- version` preserve those words as direct queries.
- Windows PowerShell, Linux, and macOS are first-class. Keep platform-specific process and terminal behavior isolated and compile-checkable.

## Code map

- `src/main.rs` — mode dispatch, user-facing success/error output, and exit status.
- `src/cli.rs` — dependency-free argument parsing and canonical `--help` text.
- `src/azure.rs` — Azure CLI discovery, process execution, JSON decoding, and command errors.
- `src/model.rs` — deserialized subscription and tenant metadata shapes.
- `src/matcher.rs` — shared ranking plus safe unique-match selection.
- `src/tui.rs` — background loading, terminal lifecycle, event handling, filtering, and rendering.
- `src/updater.rs` — Cargo-based Git update command, platform handoff, and update errors.
- `README.md` — user installation, behavior, controls, and troubleshooting.

Keep ranking shared between direct and interactive modes. A second matcher will drift.

## Azure CLI boundary

The supported command contract is:

```text
az account list --output json --only-show-errors [--refresh]
az rest --method get --url https://management.azure.com/tenants?api-version=2022-12-01 --query value[].{tenantId:tenantId,displayName:displayName,defaultDomain:defaultDomain} --output json
az account list --all --query [].{tenantId:tenantId,displayName:tenantDisplayName,defaultDomain:tenantDefaultDomain} --output json
az account set --subscription <id> --only-show-errors
```

`az account list` is intentionally used without `--all` for subscription choices, so disabled subscriptions are excluded. The `--all` metadata query must not become a subscription source or cache. `Subscription` tolerates omitted optional Azure fields, but `id` and `name` remain required. Preserve Azure CLI stderr in actionable errors without printing credentials or environment contents.

`AZC_AZ_BIN` overrides executable discovery. On Windows, default discovery must continue to support `az.exe`, `az.cmd`, and `az.bat` on `PATH`.

## Cargo update boundary

The supported self-update contract is:

```text
cargo install --git <CARGO_PKG_REPOSITORY> --locked --force
```

Execute Cargo directly without a shell. Unix waits for Cargo and propagates failure. Windows starts Cargo and exits so the running `azc.exe` does not block replacement.

## Implementation rules

- Prefer the standard library and the existing synchronous design. The one loader thread keeps startup responsive without an async runtime.
- Keep dependencies small; a new crate must remove more code or risk than it adds.
- Restore the terminal on every non-abort return path. Changes around initialization, panic handling, or event errors require a real PTY smoke check.
- Accept `KeyEventKind::Press` and repeat events while ignoring release events; Windows terminals emit release events.
- Keep every printable character available for search. Navigation shortcuts use non-printable keys, control modifiers, or `Alt`.
- Preserve deterministic ordering: match score, current subscription, case-insensitive name, then source order.
- User errors use the `azc: <detail>` stderr prefix and a nonzero exit code. Cancellation is successful and silent.
- Update `cli::help()` and `README.md` together when commands, flags, controls, environment variables, or matching semantics change.

## Verification

Tests live beside their implementation. Add tests for observable changes to parsing, update invocation, ranking, ambiguity, input handling, navigation, or Azure JSON decoding.
- Verification is a release gate, not advisory: never present a code update as ready while any required test, formatter, linter, target check, or smoke check fails.
- A reported failure is authoritative. Reproduce it when tooling is available, fix the cause, and rerun the failing check plus the complete required gate before delivery.
- If required tooling is unavailable, explicitly mark the update unverified and do not describe it as ready or complete.
- Tests for multi-tenant behavior must exercise the full JSON-to-rendered-label path, including display names for more than one tenant and duplicate or partial Azure metadata records.

Run before completion:

```sh
cargo fmt --all -- --check
cargo test
cargo clippy --all-targets -- -D warnings
```

For process discovery, terminal, or conditional-compilation changes, also run:

```sh
cargo check --target x86_64-pc-windows-gnu
cargo check --target x86_64-apple-darwin
```

For switching behavior, smoke-test both `azc <query>` and the interactive picker against a fake Azure executable supplied through `AZC_AZ_BIN`; assert the ID passed to `account set`. Never use a real subscription for automated verification.

For update behavior, place a fake `cargo` executable first on `PATH`, run `azc update`, and assert the exact `install --git ... --locked --force` arguments. Never overwrite the developer's installed `azc` during automated verification.
