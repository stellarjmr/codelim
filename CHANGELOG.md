# Changelog

All notable changes to `codelim` are tracked here.

## Unreleased

## 0.1.5 - 2026-08-11

- Fixed `--live` exiting after long runs when the Codex backend returned a temporary upstream error (for example `GET https://chatgpt.com/backend-api/wham/usage failed: 503 Service Unavailable`). Such failures arrive as RPC error responses rather than transport failures, so live mode treated them as fatal. Live mode now never exits on a failed refresh: it keeps showing the last successful snapshot, adds a one-line red `⚠ HH:MM:SS fetch failed, retrying: …` notice (truncated to avoid line wrapping breaking the in-place redraw), and retries on the next interval. The notice disappears after the next successful refresh. One-shot (non-live) runs still report such errors and exit as before.

## 0.1.4 - 2026-08-08

- Fixed intermittent `Codex app-server closed before account/rateLimits/read replied` failures by distinguishing RPC timeouts from actual disconnects, allowing more time for limit reads, and automatically restarting and reinitializing the app-server before retrying. Live mode now keeps recovering on later refreshes instead of exiting on a temporary connection failure.
- Fixed weekly-only responses being mislabeled as the 5-hour/session limit by classifying all known window durations before applying positional fallback for unknown windows.

## 0.1.3 - 2026-05-31

- Live mode now accepts a single `q` keypress to exit immediately, while keeping `Ctrl-C` as a graceful exit path that restores terminal input mode.

## 0.1.2 - 2026-05-29

- Added `--live` flag to continuously refresh the limits display in place, with `--interval <SECS>` (default 10) to control the cadence. Live mode reuses the same Codex RPC session, redraws using ANSI cursor controls (`ESC[<N>F ESC[J`), and prints a footer showing `updated HH:MM:SS · every Ns · Ctrl-C to exit`. Refuses to run when stdout is not a TTY or when combined with `--json` / `--raw`.

## 0.1.1 - 2026-05-28

- Refreshed the default text output: title + horizontal rule header, then one bar line and one indented `↻ Resets in <delta> · YYYY-MM-DD HH:MM` line per window, with no blank-line padding between sections. Bars use Unicode `▰`/`▱` and are colorized (green/yellow/red by remaining percentage) only when stdout is a TTY and `NO_COLOR` is unset. JSON and `--raw` output are unchanged.

## 0.1.0 - 2026-05-23

- Added a minimal Rust CLI that starts the local Codex CLI `app-server` and reads `account/rateLimits/read`.
- Added text output for 5-hour/session and weekly Codex limit windows with reset times.
- Added `--json`, `--raw`, `--codex-bin`, `--verbose`, `--help`, and `--version` options.
- Removed account and credits output so the CLI focuses only on Codex limits.
- Added GitHub Actions release workflow that builds only a macOS Apple Silicon binary.
- Documented Homebrew installation from the `stellarjmr/tool` tap using a prebuilt binary.
- Added repository metadata and MIT license for the public GitHub repository.
- Added project git hygiene with `.gitignore` and local development guidelines in ignored `AGENTS.md`.
