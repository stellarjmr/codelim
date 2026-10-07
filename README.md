# codelim

Minimal Rust CLI for checking OpenAI Codex and Claude Code quota windows using local logins.

By default, `codelim` shows **Codex and all discovered local Claude accounts together**. Codex uses the local CLI RPC server; Claude uses existing local credentials to query Anthropic's usage API. Each block shows the 5-hour/session and weekly limits. Claude blocks are labeled by their local directory, not email or account identity. Credits are never printed.

## Requirements

- macOS Apple Silicon for the prebuilt Homebrew package.
- Codex: OpenAI Codex CLI installed and already logged in locally, with at least one local Codex run before checking limits.
- Claude: an existing Claude Code subscription login (`/login`) in the selected configuration directory, with `user:profile` permission. Uses macOS Keychain and the system `/usr/bin/curl`; no API key, browser cookies, or separate OAuth setup is needed.

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

Text output uses a 20-cell line bar: solid `━` segments show remaining quota, and dashed `┄` segments show used quota. Reset lines use plain `Resets` without an icon. These box-drawing characters avoid the geometric-symbol font fallback that can make the previous bars look oversized; no Nerd Font or terminal configuration change is required.

Example combined output (illustrative values):

```text
  Codex limits  local Codex CLI RPC
  ──────────────────────────────────────────
  5-hour  not available
  Weekly  ━━━━━━━━━┄┄┄┄┄┄┄┄┄┄┄  43% left
          Resets in 5d 23h · 2026-09-28 14:57

  Claude limits [default]  Claude Code OAuth API
  ──────────────────────────────────────────
  5-hour  ━━━━━━━━━━━━━━━━━━━━  100% left
  Weekly  ━━━━━━━━━━┄┄┄┄┄┄┄┄┄┄  50% left

  Claude limits [~/.claude_p]  Claude Code OAuth API
  ──────────────────────────────────────────
  5-hour  ━━━━━━━━━━━━━━━━┄┄┄┄  80% left
  Weekly  ━━━━━━━━━━━━┄┄┄┄┄┄┄┄  60% left
```

Options:

```bash
codelim                             # Codex + all discovered Claude accounts
codelim --provider all              # same as the default
codelim --provider codex            # only Codex; retains the original JSON/raw schema
codelim --provider claude           # all discovered Claude accounts, without Codex
codelim --claude-config-dir /accounts/work --claude-config-dir /accounts/personal
codelim --json                      # normalized results for all accounts
codelim --raw                       # raw quota windows for all accounts
codelim --live                      # Codex refreshes every 10s, each Claude account every 180s
codelim --live --interval 240        # override every account's refresh interval
codelim --codex-bin /path/to/codex   # override Codex executable path
codelim --verbose                    # show Codex app-server stderr (no Claude credentials)
codelim --help
```

`--live` redraws all account blocks in place using ANSI cursor controls. Each account has its own refresh schedule, last successful snapshot, and error notice: a failing account does not hide other accounts or stop refreshes. The footer shows the display update time and refresh cadences. Press `q` (or `Ctrl-C`) to exit. Live mode requires TTY stdin/stdout and cannot be combined with `--json` / `--raw`.

### Claude accounts and configuration directories

Claude account discovery includes:

- The default login in `~/.claude` (the unsuffixed Keychain service).
- Home-directory siblings named `.claude-*` or `.claude_*`, such as `~/.claude-work` and `~/.claude_p`.
- Directories named by `CLAUDE_CONFIG_DIR` and `CLAUDE_SECURESTORAGE_CONFIG_DIR`, including locations outside your home directory.
- Every repeatable `--claude-config-dir` argument.

Duplicate credential-store selectors appear once. An explicitly keyed `~/.claude` Keychain login is also included when present, separately from the default login. Discovery is shallow: it does not search the whole disk or parse shell aliases. Add other locations using the **same directory string used to launch Claude Code**:

```bash
codelim --live
codelim --claude-config-dir /accounts/work --claude-config-dir /accounts/personal
CLAUDE_CONFIG_DIR=/accounts/work codelim --json
```

These directories are **additive**, not switches: all their accounts remain visible together. Environment variables never redirect every discovered account to one credential store. An empty environment value refers to the default store. `codelim` does not switch or modify your Claude login.

On macOS, credentials come from the selected `Claude Code-credentials` Keychain service. With an explicit nonempty directory, Claude adds the first eight SHA-256 hex characters of the NFC-normalized directory string to the service name. The string is **not** expanded or canonicalized: use `"$HOME/.claude-work"`, not a quoted `"~/.claude-work"`, and preserve trailing slashes if used at login. Explicitly setting `"$HOME/.claude"` selects a different Keychain item from leaving `CLAUDE_CONFIG_DIR` unset. If the selected item does not exist, `codelim` reads `.credentials.json` in that same directory; it never falls back to another account.

Claude credentials are read-only and re-read on each refresh so tokens renewed by Claude Code are picked up. `codelim` does not refresh tokens, write credentials, or run login commands. For expired credentials or missing permissions, open Claude Code with the same directory variables and run `/login`. API keys and `CLAUDE_CODE_OAUTH_TOKEN` overrides are not used: this mode reads the selected local subscription login.

Claude's usage endpoint is not a documented public API and may change or return rate-limit errors. Each Claude account refreshes every 180 seconds by default, independently of Codex's 10-second cadence. `--interval` overrides both, but shorter Claude intervals can trigger HTTP 429. Failed refreshes keep that account's last successful snapshot and retry when it is next due. One-shot runs print all results, including per-account errors, then exit nonzero if any read failed.

### JSON output

`--json` returns `{"results": [...]}` with one entry per provider/account. Successful entries contain `provider`, `source`, and `limits`; Claude entries also include a directory-based `profile`. A failed entry contains its provider/profile and `error`, without inventing zero usage. Claude uses `"provider": "claude"` and `"source": "claude-oauth-api"`; `limits.session` and `limits.weekly` contain `usedPercent`, `windowDurationMins`, and Unix-second `resetsAt` fields, or `null` when unavailable. Percentages are **used** in JSON and **remaining** in text. Reset times in text use the local timezone.

`--raw` also returns `{"results": [...]}`, with `provider`, optional Claude `profile`, and `windows` per successful entry. The windows contain only `primary` / `secondary` for Codex, or `five_hour` / `seven_day` with `utilization` and `resets_at` for Claude. Extra usage, credits, account identity, and credentials are excluded from both formats.

For existing scripts, `--provider codex --json` and `--provider codex --raw` retain the original single-object schemas rather than the combined `results` envelope.

## How Codex reads limits

Internally, `codelim` starts:

```bash
codex -s read-only app-server
```

Then it sends JSON-RPC requests to initialize the local app server and read `account/rateLimits/read`. The returned limit windows are normalized as:

- `300` minutes → 5-hour/session window
- `10080` minutes → weekly window

Known durations are classified before positional fallback, so a response containing only a weekly window remains weekly instead of being mislabeled as the 5-hour/session limit.

If the Codex app-server temporarily closes or stops responding during a limit read, `codelim` restarts it, initializes a fresh RPC session, and retries the read once. In `--live` mode, any failed refresh — including temporary backend errors such as `503 Service Unavailable` from the Codex usage endpoint — keeps the last successful snapshot on screen, shows a one-line `⚠ HH:MM:SS fetch failed, retrying: …` notice, and retries on the next interval instead of terminating the display.

## Release

Releases are built by GitHub Actions on tag pushes:

```bash
git tag v0.1.7
git push origin v0.1.7
```

The release workflow runs on `macos-14`, verifies `arm64`, builds `target/release/codelim`, and uploads `codelim-v<version>-macos-arm64.tar.gz` plus a SHA-256 checksum.

## Build from source for development

```bash
cargo build --release
```

This is for development only. Homebrew users install the prebuilt binary.
