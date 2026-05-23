# Changelog

All notable changes to `codelim` are tracked here.

## Unreleased

## 0.1.0 - 2026-05-23

- Added a minimal Rust CLI that starts the local Codex CLI `app-server` and reads `account/rateLimits/read`.
- Added text output for 5-hour/session and weekly Codex limit windows with reset times.
- Added `--json`, `--raw`, `--codex-bin`, `--verbose`, `--help`, and `--version` options.
- Removed account and credits output so the CLI focuses only on Codex limits.
- Added GitHub Actions release workflow that builds only a macOS Apple Silicon binary.
- Documented Homebrew installation from the `stellarjmr/tool` tap using a prebuilt binary.
- Added repository metadata and MIT license for the public GitHub repository.
- Added project git hygiene with `.gitignore` and local development guidelines in ignored `AGENTS.md`.
