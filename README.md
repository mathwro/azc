# azc

`azc` is a fast terminal subscription switcher for the Azure CLI. Open a searchable picker with `azc`, or switch directly with a short query such as `azc hr`.

```text
azc                  # open the interactive picker
azc hr               # switch to the unique best match for "hr"
azc human resources  # queries may contain multiple unquoted words
azc --refresh        # refresh subscriptions from Azure, then open the picker
azc --refresh hr     # refresh, then switch by query
azc --version        # print version and reproducible build metadata
azc update           # update to the latest GitHub version
```

## Why azc

Changing subscriptions with the Azure CLI normally means listing accounts, finding a name or ID, and passing it to `az account set`. `azc` keeps that workflow fast:

- The TUI is in search mode from its first frame—just start typing.
- Azure CLI loading runs in the background, so it does not block keyboard input.
- Direct mode ranks partial names predictably.
- Ambiguous direct matches fail instead of selecting the wrong subscription.
- Azure authentication stays in the installed Azure CLI; `azc` does not handle credentials or tokens.

## Requirements

- [Azure CLI](https://learn.microsoft.com/cli/azure/install-azure-cli) installed and available on `PATH`
- An authenticated Azure CLI session created with `az login`
- Windows PowerShell/Windows Terminal, a Linux terminal, or a macOS terminal
- Rust 1.88 or newer, including Cargo, when installing or using `azc update`

## Install

### Directly from GitHub

Cargo can build and install a binary directly from a Git repository:

```sh
cargo install --git https://github.com/mathwro/azc.git
```

This is the Rust equivalent of installing a Go command from its repository. It checks out the repository, builds the release binary, and places `azc` in Cargo's binary directory.

### Scoop and Homebrew

Stable `vX.Y.Z` releases provide native Windows, macOS, and Linux archives for x86-64 and ARM64. The shared Scoop bucket and Homebrew tap install those immutable GitHub Release assets without building Rust source.

### From a local checkout

From the project directory:

```sh
cargo install --path .
```

Cargo normally installs binaries to `$HOME/.cargo/bin` on Linux and macOS or `%USERPROFILE%\.cargo\bin` on Windows. Ensure that directory is on `PATH`. See the [`cargo install` reference](https://doc.rust-lang.org/cargo/commands/cargo-install.html) for Git tags, branches, and revisions.

Then authenticate and launch:

```sh
az login
azc
```

For local development without installing:

```sh
cargo run -- hr
```

## Version and updates

Print the installed version and embedded build provenance without network access:

```sh
azc --version
# azc 0.1.0 (commit 82f80fca98b1, built 2026-08-13)
```

Update to the latest version from the repository's default branch:

```sh
azc update
```

The update command runs:

```sh
cargo install --git https://github.com/mathwro/azc --locked --force
```

It therefore requires Cargo, network access to GitHub, and permission to write to Cargo's binary directory. On Windows, `azc` starts Cargo and exits so that Cargo can replace the running executable; keep the terminal open until Cargo finishes.

## Interactive picker

Run `azc` without a query. Printable characters immediately filter subscription names, subscription IDs, and tenant IDs.

| Key | Action |
| --- | --- |
| Type or paste | Filter subscriptions |
| `Up` / `Down`, `Alt+j` / `Alt+k` | Move the selection |
| `Ctrl+P` / `Ctrl+N` | Move the selection |
| `Home` / `End` | Jump to the first / last match |
| `Enter` | Switch to the selected subscription |
| `Backspace` | Remove the last query character |
| `Ctrl+U` | Clear the query |
| `Esc` / `Ctrl+C` | Cancel without changing context |

The current subscription is marked with a green dot and appears first when the search is empty. Selecting an item exits the TUI, restores the terminal, runs the context change, and prints the selected name and ID.

## Direct matching

`azc <query>` ranks case-insensitive matches in this order:

1. Exact subscription name or ID
2. Name prefix
3. Word prefix within the name
4. Name substring
5. Subscription ID or tenant ID substring

A uniquely best-ranked result is switched immediately:

```text
$ azc hr
Switched to HR Production (00000000-0000-0000-0000-000000000002)
```

If multiple subscriptions tie at the best rank, `azc` exits without changing context and lists the ambiguous names. Use a longer query or open the picker. `update` and `version` are command names; search for subscriptions with those exact names using `azc -- update` or `azc -- version`.

## Azure CLI behavior

`azc` delegates all account state to Azure CLI:

```text
az account list --output json --only-show-errors
az account set --subscription <id> --only-show-errors
```

The ordinary list command uses Azure CLI's local subscription data and returns enabled subscriptions from the current cloud. `--refresh` adds Azure CLI's `--refresh` option to retrieve current subscriptions from Azure.

See Microsoft's [`az account` reference](https://learn.microsoft.com/cli/azure/account?view=azure-cli-latest) for the underlying command behavior.

### Custom Azure CLI executable

Set `AZC_AZ_BIN` when Azure CLI is not available under its normal command name.

PowerShell:

```powershell
$env:AZC_AZ_BIN = "C:\tools\azure-cli\az.cmd"
azc hr
```

Bash or zsh:

```sh
AZC_AZ_BIN=/opt/azure-cli/bin/az azc hr
```

On Windows, default discovery checks `PATH` for `az.exe`, `az.cmd`, and `az.bat`. Linux and macOS use `az`.

## Troubleshooting

### Azure CLI executable was not found

Confirm that `az --version` works in the same terminal. Otherwise install Azure CLI, fix `PATH`, or set `AZC_AZ_BIN`.

### Azure CLI asks you to sign in

Run:

```sh
az login
```

Then retry `azc`. Authentication errors are returned directly from Azure CLI.

### A new subscription is missing

Ask Azure CLI to refresh its account data:

```sh
azc --refresh
```

### A direct query is ambiguous

Add enough of the subscription name to produce one best-ranked match, or run `azc` and choose interactively.

## Project layout

```text
src/
├── azure.rs     Azure CLI process boundary and error reporting
├── cli.rs       Argument parsing and help output
├── main.rs      Mode dispatch and exit behavior
├── matcher.rs   Shared ranking and unique-match selection
├── model.rs     Azure subscription JSON model
├── tui.rs       Background loading, input handling, and rendering
├── updater.rs   Cargo-based updates and update error reporting
└── version.rs   Embedded release version and build provenance
```

## Development

Run the complete local quality gate:

```sh
cargo fmt --all -- --check
cargo test
cargo clippy --all-targets -- -D warnings
```

Platform-sensitive changes can be compile-checked from a configured Rust toolchain:

```sh
cargo check --target x86_64-pc-windows-gnu
cargo check --target x86_64-apple-darwin
```

The project-specific contribution and verification contracts are in [`AGENTS.md`](AGENTS.md).

## Releasing

`release.toml` is the canonical package and target metadata. Releases require an annotated or signed tag named exactly `vX.Y.Z`, where `X.Y.Z` equals the version in `Cargo.toml`. Before tagging, merge the release commit to protected `main`, pass the `CI` required check, and configure the `release` environment for maintainer approval.

Dry-run the exact tagged source before publication:

1. Create a temporary annotated prerelease tag, for example `v0.1.0-rc.1`, on a commit whose `Cargo.toml` version is also `0.1.0-rc.1`.
2. Push the tag.
3. Run the `Release` workflow manually with that tag and `dry_run: true`.
4. Confirm all six `release-*` artifacts and the complete staging artifact pass. No GitHub Release or package dispatch is created.

Publish a stable release:

```sh
git switch main
git pull --ff-only
git tag -a v0.1.0 -m "azc v0.1.0"
git push origin v0.1.0
```

The tag workflow builds with locked dependencies on six native GitHub-hosted runners, embeds the tagged commit and commit date, checks each root-only archive, generates and independently verifies `checksums.txt`, creates a draft, compares remote names and sizes, then publishes it. A successful publication dispatches one `package-release` event to `mathwro/homebrew-tools` with `repository`, `tag`, and `release_id`. Configure `DISTRIBUTION_REPOSITORY_TOKEN` as an environment secret scoped only to dispatch that repository.

Release assets are immutable. Never move or recreate a consumed tag, and never replace an asset on a published release. For a defective asset, fix the source and publish a new patch version. An interrupted unpublished draft may be deleted and rerun only after confirming it was never published. A failed package dispatch does not alter the release; resolve the credential or destination issue and let distribution reconciliation rediscover it rather than republishing.

Release binaries have these platform properties:

- Linux archives use static musl binaries, avoiding a host libc dependency.
- macOS archives are currently unsigned. Homebrew's GitHub download path must verify Gatekeeper behavior before the first stable package update; add repository-approved signing later without changing archive names.
- Windows archives are native `.exe` files and require no installer, registry writes, administrator access, or profile changes.

