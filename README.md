# codelim

Minimal Rust CLI for checking local OpenAI Codex quota windows.

`codelim` starts the local Codex CLI RPC server, reads `account/rateLimits/read`, and prints only the 5-hour/session and weekly limit windows. It does not print account identity or credits.

## Requirements

- macOS Apple Silicon for the prebuilt Homebrew package.
- OpenAI Codex CLI installed and already logged in locally.
- At least one local Codex run before checking limits.

## Install with Homebrew

```bash
brew tap stellarjmr/tool
brew install stellarjmr/tool/codelim
```

The Homebrew formula installs a prebuilt macOS Apple Silicon binary from GitHub Releases. It does not build from source and does not require Rust.

## Run

```bash
codelim
```

Options:

```bash
codelim --json                       # normalized JSON output
codelim --raw                        # raw Codex limit-window fields as JSON
codelim --live                       # continuously refresh in place; press q to exit (TTY only, default every 10s)
codelim --live --interval 3          # refresh every 3 seconds; press q to exit
codelim --codex-bin /path/to/codex   # override Codex executable path
codelim --help
```

`--live` redraws the same block of lines in place using ANSI cursor controls and prints a footer with the last update time and refresh interval. Press `q` (or `Ctrl-C`) to exit. It is rejected when stdout or stdin is not a TTY, or when combined with `--json` / `--raw`.

## What it does

Internally, `codelim` starts:

```bash
codex -s read-only -a untrusted app-server
```

Then it sends JSON-RPC requests to initialize the local app server and read `account/rateLimits/read`. The returned limit windows are normalized as:

- `300` minutes → 5-hour/session window
- `10080` minutes → weekly window

If the Codex app-server temporarily closes or stops responding during a limit read, `codelim` restarts it, initializes a fresh RPC session, and retries the read once. In `--live` mode, a connection failure that remains after that retry is tried again on the next refresh instead of terminating the display.

## Release

Releases are built by GitHub Actions on tag pushes:

```bash
git tag v0.1.3
git push origin v0.1.3
```

The release workflow runs on `macos-14`, verifies `arm64`, builds `target/release/codelim`, and uploads `codelim-v<version>-macos-arm64.tar.gz` plus a SHA-256 checksum.

## Build from source for development

```bash
cargo build --release
```

This is for development only. Homebrew users install the prebuilt binary.
