# Changelog

All notable changes to `codelim` are tracked here.

## Unreleased

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
