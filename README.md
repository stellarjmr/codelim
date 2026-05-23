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
codelim --json
codelim --raw
codelim --codex-bin /path/to/codex
codelim --help
```

## What it does

Internally, `codelim` starts:

```bash
codex -s read-only -a untrusted app-server
```

Then it sends JSON-RPC requests to initialize the local app server and read `account/rateLimits/read`. The returned limit windows are normalized as:

- `300` minutes → 5-hour/session window
- `10080` minutes → weekly window

## Release

Releases are built by GitHub Actions on tag pushes:

```bash
git tag v0.1.0
git push origin v0.1.0
```

The release workflow runs on `macos-14`, verifies `arm64`, builds `target/release/codelim`, and uploads `codelim-v<version>-macos-arm64.tar.gz` plus a SHA-256 checksum.

## Build from source for development

```bash
cargo build --release
```

This is for development only. Homebrew users install the prebuilt binary.
