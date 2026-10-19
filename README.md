# azc

`azc` is a fast terminal subscription and tenant switcher for the Azure CLI. Open a searchable subscription picker with `azc`, or switch directly with a short query such as `azc hr`.

```text
azc                  # open the interactive subscription picker
azc hr               # switch to the unique best match for "hr"
azc human resources  # queries may contain multiple unquoted words
azc tenant           # open the interactive tenant picker
azc tenant 0000      # switch to the unique best tenant-ID match
azc --refresh        # refresh subscriptions from Azure, then open the picker
azc --refresh hr     # refresh, then switch by query
azc update           # update to the latest GitHub version
azc --version        # print version and reproducible build metadata
```

## Why azc

- The TUI shows the active tenant in its title and footer.
- `Ctrl+T` toggles between the subscription list and a tenant list.
- The tenant picker groups enabled subscriptions by tenant and marks the active tenant.
- Selecting a tenant opens its subscriptions; only selecting a subscription changes Azure CLI context.
- The TUI is in search mode from its first frame—just start typing.
- Azure CLI loading runs in the background, so it does not block keyboard input.
- Recent picker results appear immediately while Azure CLI updates them in the background.
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

Run `azc` without a query. Printable characters immediately filter subscription names, subscription IDs, and tenant IDs. The footer shows the current tenant ID once account data is available; cached markers reflect the last snapshot until the background listing finishes.

| Key | Action |
| --- | --- |
| Type or paste | Filter the active list |
| `Up` / `Down`, `Alt+j` / `Alt+k` | Move the selection |
| `Ctrl+P` / `Ctrl+N` | Move the selection |
| `Ctrl+T` | Toggle between subscriptions and tenants |
| `Ctrl+R` | Refresh subscriptions from Azure without leaving the picker |
| `Home` / `End` | Jump to the first / last match |
| `Enter` | Open the selected tenant's subscriptions, or switch to the selected subscription |
| `Backspace` | Remove the last query character |
| `Ctrl+U` | Clear the query |
| `Esc` / `Ctrl+C` | Cancel without changing context |

The subscription list marks the current subscription with a green dot. Tenant mode groups enabled subscriptions by tenant, marks the current tenant, and shows its subscription count. Each tenant uses `Display name (domain)` with the tenant ID on a muted second line. Missing, blank, or GUID-only display names fall back to the domain, then the tenant ID when no friendly metadata is available.

Press `Enter` on a tenant to open only that tenant's subscriptions, with a fresh search field. This does not change Azure CLI context or exit the picker—even when the tenant has only one subscription. Choose a subscription and press `Enter` again to switch by its ID, restore the terminal, and print the selected context. `Ctrl+T` returns to the tenant list; toggling again opens all subscriptions. `Esc` and `Ctrl+C` cancel from either list without switching.

The subscription and tenant pickers first show the last successful subscription list if it is no more than seven days old, marked **cached · updating…**. Azure CLI then reloads subscriptions in the background, followed by tenant names. On a first run, subscriptions become searchable without waiting for tenant metadata.

Updates preserve your search text and highlighted subscription ID, or highlighted tenant ID in tenant mode. After opening a tenant, searches and background refreshes remain restricted to that tenant's subscriptions. If the selected subscription disappears or no longer matches, the selection clears: navigate or edit the search before pressing Enter. You can select a cached subscription before updating finishes; Azure CLI handles the switch by ID. If Azure CLI rejects that context as missing or requests authentication, `azc` offers sign-in before retrying that same ID, never a replacement. If background listing fails, cached results remain available with an error indicator.

Press `Ctrl+R` in either picker to request a server refresh, just like `--refresh`. The current list stays usable while subscriptions and tenant names reload, and the successful results update the cache. If startup loading is still running, one refresh is queued behind it and superseded startup results are ignored. Further `Ctrl+R` presses are ignored until that refresh finishes. A failed refresh keeps the existing list and shows the error; press `Ctrl+R` again to retry. Refreshing itself never changes Azure context.

The snapshot contains subscription IDs, names, tenant IDs, last-seen active markers, and tenant labels—not credentials or tokens. It is stored as `azc-picker-cache.json` inside `AZURE_CONFIG_DIR`, or `~/.azure` by default, and is scoped to the Azure CLI executable override. Missing, expired, incompatible, or corrupt snapshots are ignored; an unwritable cache does not prevent normal loading. Delete that file to reset the picker cache.

`azc --refresh` and `azc tenant --refresh` bypass cached results and explicitly refresh subscriptions from Azure. Direct queries (`azc <query>` and `azc tenant <query>`) always use Azure CLI output, never the picker snapshot. A recent snapshot can still be stale after signing in as another user or changing context elsewhere; use `--refresh` when you need to start from refreshed account data.

## Direct matching

`azc <query>` ranks case-insensitive subscription matches in this order:

1. Exact subscription name or ID
2. Name prefix
3. Word prefix within the name
4. Name substring
5. Subscription ID or tenant ID substring

`azc tenant <query>` applies exact, prefix, and substring matching to tenant display names, default domains, and IDs. A uniquely best-ranked tenant is switched by selecting one of its subscriptions:

```text
$ azc tenant 1000
Switched to tenant 10000000-0000-0000-0000-000000000001 via HR Production (00000000-0000-0000-0000-000000000002)
```

If multiple subscriptions or tenants tie at the best rank, `azc` exits without changing context and lists the ambiguous values. Use a longer query, or open the appropriate picker. `update`, `version`, and `tenant` are command names; search for subscriptions with those exact names using `azc -- update`, `azc -- version`, or `azc -- tenant`.

## Azure CLI behavior

`azc` delegates all account state to Azure CLI:

```text
az account list --output json --only-show-errors [--refresh]
az rest --method get --url "https://management.azure.com/tenants?api-version=2022-12-01" --query "value[].{tenantId:tenantId,displayName:displayName,defaultDomain:defaultDomain}" --output json
az account list --all --query "[].{tenantId:tenantId,displayName:tenantDisplayName,defaultDomain:tenantDefaultDomain}" --output json
az account set --subscription <id> --only-show-errors
az login [--tenant <tenant-id>] --output none
```

The ordinary subscription list uses Azure CLI's local account data. `--refresh` adds Azure CLI's `--refresh` option to retrieve current subscriptions from Azure. The tenant picker gets tenant IDs, display names, and default domains from the Azure Resource Manager tenants endpoint through `az rest`, then fills missing metadata from the filtered local `az account list --all` query. If tenant metadata is unavailable, it falls back to tenant IDs. Switching a tenant means selecting one of its subscriptions by ID; Azure CLI then makes that subscription and its tenant active.

Azure CLI child processes receive `PYTHONIOENCODING=utf-8` so subscription and tenant labels retain their Unicode characters. The official Windows launcher uses an isolated Python interpreter that ignores this variable; when its output is not valid UTF-8, `azc` decodes it using the Windows system ANSI code page before parsing JSON. This also supports Linux `azc` in WSL invoking Windows Azure CLI through a Scoop or other interop shim: a fixed, noninteractive `powershell.exe` command queries the Windows ANSI code page once per run, only when non-UTF-8 output requires it. Missing interop tooling, unsupported encodings, invalid JSON, or missing required subscription fields still produce errors; characters are not silently replaced. This does not change your terminal, system locale, or Azure CLI installation.

If switching fails because the selected subscription is missing from Azure CLI's account data or Azure CLI requests authentication, `azc` restores the terminal and asks whether to sign in and retry. Sign-in is opt-in (`[y/N]`); Enter or any answer other than `y`/`yes` cancels successfully without starting login. This applies to subscription and tenant selections, in both direct and picker modes.

After confirmation, `azc` runs `az login --tenant <tenant-id> --output none` (omitting `--tenant` when metadata is unavailable), with Azure CLI's normal browser/MFA instructions visible. It disables Azure CLI's subscription selector for that child process only, without changing saved configuration, then retries the originally selected subscription ID once. Login may update Azure CLI's account data and defaults. A failed login stops recovery; a failed retry reports Azure CLI's error without prompting again or selecting another subscription. Unrelated switching errors are still reported directly.

When stdin or stderr is not a terminal, `azc` never prompts or starts login. It exits nonzero with the original diagnostic and a tenant-specific `az login` instruction instead.

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

If this happens while switching to a selected subscription or tenant, accept `azc`'s sign-in prompt and complete Azure CLI's browser/MFA flow. A missing-subscription error can also mean access was removed or the active Azure cloud is wrong; signing in is a recovery option, not proof that a session expired.

For errors during listing, or when running without a terminal, sign in manually and retry:

```sh
az login --tenant <tenant-id>
azc
```

Omit `--tenant` if the tenant ID is unknown. See Microsoft's [interactive sign-in guidance](https://learn.microsoft.com/cli/azure/authenticate-azure-cli-interactively?view=azure-cli-latest) for browser, MFA, and device-code options.

### `invalid unicode code point` while loading accounts

This is an output-encoding failure, not an expired-session message. Older `azc` builds assumed all Azure CLI JSON was UTF-8, but Windows Azure CLI can emit legacy-encoded bytes when its output is captured, including when invoked from WSL. Rebuild or update `azc` to use the decoding behavior described above. Custom `AZC_AZ_BIN` launchers should emit UTF-8, or the Windows system ANSI code page when invoking Windows Azure CLI. WSL interop requires `powershell.exe` on `PATH` to identify that code page.

### A new subscription is missing

Ask Azure CLI to refresh its account data:

```sh
azc --refresh
```

### A direct query is ambiguous

For subscriptions, add enough of the name to produce one best-ranked match, or run `azc` and choose interactively. For tenants, add more of the tenant ID or run `azc tenant`.

## Project layout

```text
src/
├── azure.rs     Azure CLI process boundary and error reporting
├── cache.rs     Best-effort interactive subscription and tenant snapshots
├── cli.rs       Argument parsing and help output
├── main.rs      Mode dispatch and exit behavior
├── matcher.rs   Shared ranking and unique-match selection
├── model.rs     Azure subscription and tenant models
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

`release.json` selects the typed Rust adapter and stable package/binary identity. The shared `mathwro/homebrew-tools/.github/workflows/release-tool.yml@main` workflow owns Rust testing, target setup, native builds, packaging, checksums, publication, and distribution notification. The release tag version must equal the version in `Cargo.toml`.

Before the first release, merge the shared workflow into `mathwro/homebrew-tools`, require the `CI` and `Workflow Lint` checks in `main` branch protection, create the protected `release` environment with required reviewers, and configure the `DISTRIBUTION_DISPATCH_TOKEN` repository secret. The token must be limited to sending repository dispatches to `mathwro/homebrew-tools`.

Workflow lint runs on every pull request and push to `main`, without path filters, so its required branch-protection check is never skipped for source-only changes.

For a complete dry run, set `Cargo.toml` to a prerelease version such as `0.1.0-rc.1`, merge it, and push the matching annotated tag. Prerelease tag pushes automatically run with `dry_run: true`: all six native archives and the complete staging artifact are produced, but no GitHub Release or package update is created. The workflow can also be dispatched manually for an existing tag.

Publish a stable release:

```sh
git switch main
git pull --ff-only
git tag -a v0.4.0 -m "azc v0.4.0"
git push origin v0.4.0
```

The stable tag run repeats the dry-run checks with locked dependencies and waits at the protected `release` environment before creating and publishing the immutable GitHub Release. Stable publication then calls the central notifier, which verifies the exact release and sends the canonical package update event.

Release assets are immutable. Never move or recreate a consumed tag, and never replace an asset on a published release. For a defective artifact, fix the source and publish a new patch version. A notification failure does not invalidate the release; distribution reconciliation discovers it without republishing.

Release binaries have these platform properties:

- Linux archives use static musl binaries, avoiding a host libc dependency.
- macOS archives are currently unsigned. Homebrew's GitHub download path must verify Gatekeeper behavior before the first stable package update; add repository-approved signing later without changing archive names.
- Windows archives are native `.exe` files and require no installer, registry writes, administrator access, or profile changes.

