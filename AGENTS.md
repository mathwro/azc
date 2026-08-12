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
- Windows PowerShell, Linux, and macOS are first-class. Keep platform-specific process and terminal behavior isolated and compile-checkable.

## Code map

- `src/main.rs` — mode dispatch, user-facing success/error output, and exit status.
- `src/cli.rs` — dependency-free argument parsing and canonical `--help` text.
- `src/azure.rs` — Azure CLI discovery, process execution, JSON decoding, and command errors.
- `src/model.rs` — deserialized `az account list` subscription shape.
- `src/matcher.rs` — shared ranking plus safe unique-match selection.
- `src/tui.rs` — background loading, terminal lifecycle, event handling, filtering, and rendering.
- `README.md` — user installation, behavior, controls, and troubleshooting.

Keep ranking shared between direct and interactive modes. A second matcher will drift.

## Azure CLI boundary

The supported command contract is:

```text
az account list --output json --only-show-errors [--refresh]
az account set --subscription <id> --only-show-errors
```

`az account list` is intentionally used without `--all`, so disabled subscriptions are excluded. `Subscription` tolerates omitted optional Azure fields, but `id` and `name` remain required. Preserve Azure CLI stderr in actionable errors without printing credentials or environment contents.

`AZC_AZ_BIN` overrides executable discovery. On Windows, default discovery must continue to support `az.exe`, `az.cmd`, and `az.bat` on `PATH`.

## Implementation rules

- Prefer the standard library and the existing synchronous design. The one loader thread keeps startup responsive without an async runtime.
- Keep dependencies small; a new crate must remove more code or risk than it adds.
- Restore the terminal on every non-abort return path. Changes around initialization, panic handling, or event errors require a real PTY smoke check.
- Accept `KeyEventKind::Press` and repeat events while ignoring release events; Windows terminals emit release events.
- Keep every printable character available for search. Navigation shortcuts use non-printable keys, control modifiers, or `Alt`.
- Preserve deterministic ordering: match score, current subscription, case-insensitive name, then source order.
- User errors use the `azc: <detail>` stderr prefix and a nonzero exit code. Cancellation is successful and silent.
- Update `cli::help()` and `README.md` together when flags, controls, environment variables, or matching semantics change.

## Verification

Tests live beside their implementation. Add tests for observable changes to parsing, ranking, ambiguity, input handling, navigation, or Azure JSON decoding.

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
