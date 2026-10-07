use crate::{cli_error, LimitRead, LimitSummary, RateWindow, Result, Snapshot, APP_VERSION};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use unicode_normalization::UnicodeNormalization;

const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const LOGIN_HINT: &str =
    "open Claude Code with the same CLAUDE_CONFIG_DIR / CLAUDE_SECURESTORAGE_CONFIG_DIR and run /login";

pub(super) struct ClaudeClient {
    pub(super) profile: String,
    credentials_path: PathBuf,
    keychain_service: String,
}

impl ClaudeClient {
    pub(super) fn discover(extra_dirs: &[String]) -> Result<Vec<Self>> {
        let config_env = read_directory_env("CLAUDE_CONFIG_DIR")?;
        let storage_env = read_directory_env("CLAUDE_SECURESTORAGE_CONFIG_DIR")?;
        let home = env::var_os("HOME");
        Self::discover_in(
            home.as_deref().map(Path::new),
            extra_dirs
                .iter()
                .map(String::as_str)
                .chain(config_env.as_deref())
                .chain(storage_env.as_deref()),
            keychain_exists,
        )
    }

    fn discover_in<'a>(
        home: Option<&Path>,
        extra_dirs: impl Iterator<Item = &'a str>,
        keychain_exists: impl Fn(&str) -> bool,
    ) -> Result<Vec<Self>> {
        let mut clients = vec![Self::for_directories(None, None, home)?];
        // Explicit/environment selectors retain their exact spelling for the
        // Keychain hash. They add accounts instead of redirecting other stores.
        for directory in extra_dirs {
            let client = Self::for_directories(Some(directory), None, home)?;
            if !clients
                .iter()
                .any(|existing| existing.keychain_service == client.keychain_service)
            {
                clients.push(client);
            }
        }
        if let Some(home) = home {
            let mut directories = fs::read_dir(home)
                .map_err(|_| cli_error("cannot scan HOME for Claude account directories"))?
                .filter_map(std::result::Result::ok)
                .map(|entry| entry.path())
                .filter(|path| {
                    path.is_dir()
                        && path
                            .file_name()
                            .and_then(|name| name.to_str())
                            .is_some_and(|name| {
                                name == ".claude"
                                    || name.starts_with(".claude-")
                                    || name.starts_with(".claude_")
                            })
                })
                .collect::<Vec<_>>();
            directories.sort();
            for directory in directories {
                let Some(selector) = directory.to_str() else {
                    continue;
                };
                let client = Self::for_directories(Some(selector), None, Some(home))?;
                // The explicit default directory is a different Keychain slot.
                // Include it only if it exists, not as a second file fallback.
                if directory == home.join(".claude") && !keychain_exists(&client.keychain_service) {
                    continue;
                }
                if !clients
                    .iter()
                    .any(|existing| existing.keychain_service == client.keychain_service)
                {
                    clients.push(client);
                }
            }
        }
        Ok(clients)
    }

    fn for_directories(
        config_dir: Option<&str>,
        storage_dir: Option<&str>,
        home: Option<&Path>,
    ) -> Result<Self> {
        // Claude hashes the NFC-normalized selector, not a resolved path. In
        // particular, explicit ~/.claude differs from an unset selector, and
        // an empty secure-storage override deliberately selects the default.
        // https://github.com/anthropics/claude-code/issues/79223
        let selector = storage_dir.or(config_dir).unwrap_or("");
        let (directory, keychain_service) = if selector.is_empty() {
            (
                home.ok_or_else(|| cli_error("HOME is required for the default Claude directory"))?
                    .join(".claude"),
                "Claude Code-credentials".to_string(),
            )
        } else {
            let normalized: String = selector.nfc().collect();
            let hash = format!("{:x}", Sha256::digest(normalized.as_bytes()));
            (
                PathBuf::from(selector),
                format!("Claude Code-credentials-{}", &hash[..8]),
            )
        };
        Ok(Self {
            profile: if selector.is_empty() {
                "default".to_string()
            } else if let Some(relative) =
                home.and_then(|home| selector.strip_prefix(&format!("{}/", home.display())))
            {
                format!("~/{relative}")
            } else {
                selector.to_string()
            },
            credentials_path: directory.join(".credentials.json"),
            keychain_service,
        })
    }

    pub(super) fn fetch(&self) -> Result<LimitRead> {
        // Re-read on every refresh: Claude Code may have renewed the token.
        let token = self.read_access_token(read_keychain)?;
        let mut child = usage_command()
            .spawn()
            .map_err(|_| cli_error("failed to start /usr/bin/curl for Claude usage"))?;

        // The bearer token goes through stdin, never argv, a file, or stderr.
        let written = match child.stdin.take() {
            Some(mut stdin) => writeln!(stdin, "Authorization: Bearer {token}"),
            None => Err(std::io::Error::other("missing curl stdin")),
        };
        let output = child
            .wait_with_output()
            .map_err(|_| cli_error("failed to read the Claude usage request result"))?;
        if written.is_err() || !output.status.success() {
            // Do not forward curl's stderr or an upstream body: either could
            // contain authentication details. curl enforces a 10-second limit.
            return Err(cli_error(
                "Claude usage request failed or timed out; check your network and retry",
            ));
        }
        let response = std::str::from_utf8(&output.stdout)
            .map_err(|_| cli_error("Claude usage API returned invalid text"))?;
        let (body, status) = response
            .rsplit_once('\n')
            .ok_or_else(|| cli_error("Claude usage API returned no HTTP status"))?;
        parse_usage_response(status, body)
    }

    fn read_access_token(
        &self,
        keychain: impl FnOnce(&str) -> Result<Option<Vec<u8>>>,
    ) -> Result<String> {
        let bytes = match keychain(&self.keychain_service)? {
            Some(bytes) => bytes,
            None => fs::read(&self.credentials_path).map_err(|error| {
                if error.kind() == std::io::ErrorKind::NotFound {
                    cli_error(format!(
                        "no Claude credentials for the selected directory; {LOGIN_HINT}"
                    ))
                } else {
                    cli_error("cannot read Claude .credentials.json for the selected directory")
                }
            })?,
        };
        parse_access_token(&bytes, Utc::now().timestamp_millis())
    }
}

fn read_directory_env(name: &str) -> Result<Option<String>> {
    match env::var(name) {
        Ok(value) => Ok(Some(value)),
        Err(env::VarError::NotPresent) => Ok(None),
        Err(env::VarError::NotUnicode(_)) => {
            Err(cli_error(format!("{name} must be valid Unicode")))
        }
    }
}

fn keychain_exists(service: &str) -> bool {
    cfg!(target_os = "macos")
        && Command::new("/usr/bin/security")
            .args(["find-generic-password", "-s", service])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
}

fn read_keychain(service: &str) -> Result<Option<Vec<u8>>> {
    if !cfg!(target_os = "macos") {
        return Ok(None);
    }
    let output = Command::new("/usr/bin/security")
        .args(["find-generic-password", "-s", service, "-w"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map_err(|_| cli_error("failed to read Claude credentials from macOS Keychain"))?;
    if output.status.success() {
        Ok(Some(output.stdout))
    } else if output.status.code() == Some(44) {
        // errSecItemNotFound; only fall back to this same profile's file.
        Ok(None)
    } else {
        Err(cli_error(
            "cannot read Claude credentials; unlock/allow access to the selected macOS Keychain item",
        ))
    }
}

#[derive(Deserialize)]
struct Credentials {
    #[serde(rename = "claudeAiOauth")]
    oauth: Option<OAuthCredentials>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct OAuthCredentials {
    access_token: String,
    expires_at: Option<i64>,
    scopes: Option<Vec<String>>,
}

fn parse_access_token(bytes: &[u8], now_millis: i64) -> Result<String> {
    // Omit serde's error text, which can quote values from credential data.
    let credentials: Credentials = serde_json::from_slice(bytes)
        .map_err(|_| cli_error(format!("invalid Claude credential data; {LOGIN_HINT}")))?;
    let oauth = credentials.oauth.ok_or_else(|| {
        cli_error(format!(
            "Claude subscription OAuth credentials are required (not an API key); {LOGIN_HINT}"
        ))
    })?;
    if oauth.access_token.is_empty() || !oauth.access_token.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(cli_error(format!(
            "invalid Claude access token; {LOGIN_HINT}"
        )));
    }
    if oauth
        .expires_at
        .is_some_and(|expires| expires <= now_millis)
    {
        return Err(cli_error(format!(
            "Claude access token expired; {LOGIN_HINT}"
        )));
    }
    if oauth
        .scopes
        .is_some_and(|scopes| !scopes.iter().any(|scope| scope == "user:profile"))
    {
        return Err(cli_error(format!(
            "Claude credentials lack user:profile permission to read usage; {LOGIN_HINT}"
        )));
    }
    Ok(oauth.access_token)
}

fn usage_command() -> Command {
    let mut command = Command::new("/usr/bin/curl");
    command
        .args([
            "--disable", // Ignore ~/.curlrc (redirects, logging, extra URLs, etc.).
            "--silent",
            "--show-error",
            "--proto",
            "=https",
            "--connect-timeout",
            "5",
            "--max-time",
            "10",
            "--max-filesize",
            "1048576",
            "--header",
            "@-",
            "--header",
            "Accept: application/json",
            "--header",
            "Content-Type: application/json",
            "--header",
            "anthropic-beta: oauth-2025-04-20",
            "--user-agent",
            &format!("codelim/{APP_VERSION}"),
            "--write-out",
            "\n%{http_code}",
            "--url",
            USAGE_URL,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

#[derive(Deserialize, Serialize)]
struct UsageResponse {
    five_hour: Option<UsageWindow>,
    seven_day: Option<UsageWindow>,
}

#[derive(Deserialize, Serialize)]
struct UsageWindow {
    utilization: Option<f64>,
    resets_at: Option<String>,
}

impl UsageWindow {
    fn into_window(self, duration_mins: i64) -> Result<Option<RateWindow>> {
        let Some(used_percent) = self.utilization else {
            return Ok(None);
        };
        let resets_at = self
            .resets_at
            .map(|time| {
                DateTime::parse_from_rfc3339(&time)
                    .map(|time| time.timestamp())
                    .map_err(|_| cli_error("Claude usage API returned an invalid reset timestamp"))
            })
            .transpose()?;
        Ok(Some(RateWindow {
            used_percent,
            window_duration_mins: Some(duration_mins),
            resets_at,
        }))
    }
}

fn parse_usage_response(status: &str, body: &str) -> Result<LimitRead> {
    match status {
        "200" => {}
        "401" => return Err(cli_error(format!("Claude authentication expired or was rejected; {LOGIN_HINT}"))),
        "403" => return Err(cli_error(format!("Claude usage access denied; a subscription login with user:profile permission is required; {LOGIN_HINT}"))),
        "429" => return Err(cli_error("Claude usage API rate limited (HTTP 429); wait before retrying and use --interval 180 or longer")),
        _ => {
            let code: u16 = status.parse().map_err(|_| cli_error("invalid Claude HTTP status"))?;
            return Err(cli_error(format!("Claude usage API failed (HTTP {code}); retry later")));
        }
    }
    let response: UsageResponse = serde_json::from_str(body)
        .map_err(|_| cli_error("Claude usage API returned invalid limit data"))?;
    // Allowlist only the two quota windows. Never serialize extra_usage,
    // credits, account metadata, or any other fields returned by the API.
    let raw = serde_json::to_value(&response)?;
    let session = response
        .five_hour
        .map(|window| window.into_window(300))
        .transpose()?
        .flatten();
    let weekly = response
        .seven_day
        .map(|window| window.into_window(10080))
        .transpose()?
        .flatten();
    Ok(LimitRead {
        snapshot: Snapshot {
            provider: "claude",
            source: "claude-oauth-api",
            limits: LimitSummary { session, weekly },
        },
        raw,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn client(config: Option<&str>, storage: Option<&str>) -> ClaudeClient {
        ClaudeClient::for_directories(config, storage, Some(Path::new("/Users/test"))).unwrap()
    }

    #[test]
    fn discovers_all_local_profiles_and_deduplicates_explicit_directories() {
        let home = env::temp_dir().join(format!("codelim-discovery-{}", std::process::id()));
        fs::create_dir(&home).unwrap();
        for name in [
            ".claude",
            ".claude_p",
            ".claude-work",
            "unrelated",
            ".claudecache",
        ] {
            fs::create_dir(home.join(name)).unwrap();
        }
        fs::write(home.join(".claude_file"), "not a directory").unwrap();
        let personal = home.join(".claude_p");
        let selectors = [
            "/external/claude",
            personal.to_str().unwrap(),
            "/external/claude",
            "",
        ];
        let profiles =
            ClaudeClient::discover_in(Some(&home), selectors.into_iter(), |_| false).unwrap();
        assert_eq!(
            profiles
                .iter()
                .map(|client| client.profile.as_str())
                .collect::<Vec<_>>(),
            [
                "default",
                "/external/claude",
                "~/.claude_p",
                "~/.claude-work"
            ]
        );
        assert_eq!(
            profiles[2].credentials_path,
            personal.join(".credentials.json")
        );

        // An explicit default-directory Keychain login is not the bare service.
        let profiles =
            ClaudeClient::discover_in(Some(&home), std::iter::empty(), |_| true).unwrap();
        assert_eq!(
            profiles
                .iter()
                .map(|client| client.profile.as_str())
                .collect::<Vec<_>>(),
            ["default", "~/.claude", "~/.claude-work", "~/.claude_p"]
        );
        assert_ne!(profiles[0].keychain_service, profiles[1].keychain_service);
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn independent_account_results_keep_stale_data_and_clear_only_the_recovered_error() {
        let mut first = crate::LimitEntry::new(
            crate::LimitClient::Claude(client(Some("/Users/test/.claude_p"), None)),
            None,
        );
        let mut second =
            crate::LimitEntry::new(crate::LimitClient::Claude(client(None, None)), Some(240));
        assert_eq!(first.interval.as_secs(), 180);
        assert_eq!(second.interval.as_secs(), 240);

        first.record(parse_usage_response(
            "200",
            r#"{"seven_day":{"utilization":57}}"#,
        ));
        first.record(Err(cli_error("HTTP 503 fixture")));
        second.record(parse_usage_response(
            "200",
            r#"{"five_hour":{"utilization":21}}"#,
        ));
        let mut entries = [first, second];
        let text = crate::render_entries(&entries, false, true);
        assert!(text.contains("Claude limits [~/.claude_p]"));
        assert!(text.contains("Claude limits [default]"));
        assert!(text.contains("43% left"));
        assert!(text.contains("79% left"));
        assert_eq!(text.matches("fetch failed, retrying").count(), 1);

        let first = entries[0].json(false).unwrap();
        assert_eq!(first["profile"], "~/.claude_p");
        assert_eq!(first["limits"]["weekly"]["usedPercent"], 57.0);
        assert_eq!(first["error"], "HTTP 503 fixture");
        let second = entries[1].json(true).unwrap();
        assert_eq!(second["provider"], "claude");
        assert_eq!(second["windows"]["five_hour"]["utilization"], 21.0);
        assert!(second.get("error").is_none());

        entries[0].record(parse_usage_response(
            "200",
            r#"{"seven_day":{"utilization":62}}"#,
        ));
        assert!(entries[0].error.is_none());
        assert!(crate::render_entries(&entries, false, true).contains("38% left"));
        // A Codex refresh must not cause another Claude HTTP call before due.
        entries[0].next_refresh = std::time::Instant::now() + std::time::Duration::from_secs(180);
        entries[0].refresh_if_due();
        assert!(entries[0].error.is_none());
        assert_eq!(
            entries[0].json(false).unwrap()["limits"]["weekly"]["usedPercent"],
            62.0
        );
    }

    #[test]
    fn failed_codex_startup_does_not_hide_claude_or_its_profile() {
        let mut codex = crate::LimitEntry::new(
            crate::LimitClient::Codex {
                codex_bin: "/nonexistent/codelim-test-codex".to_string(),
                verbose: false,
                session: None,
            },
            None,
        );
        assert_eq!(codex.interval.as_secs(), 10);
        codex.refresh_if_due();
        assert!(codex.error.is_some());
        let mut claude =
            crate::LimitEntry::new(crate::LimitClient::Claude(client(None, None)), None);
        claude.record(parse_usage_response(
            "200",
            r#"{"seven_day":{"utilization":9}}"#,
        ));
        let text = crate::render_entries(&[codex, claude], false, false);
        assert!(text.contains("Codex limits"));
        assert!(text.contains("failed to start"));
        assert!(text.contains("Claude limits [default]"));
        assert!(text.contains("91% left"));
    }

    #[test]
    fn matches_claude_keychain_selector_hashes_without_path_normalization() {
        // Expected hashes computed independently with /usr/bin/shasum -a 256.
        for (selector, suffix) in [
            ("/Users/test/.claude", "462977e4"),
            ("/tmp/claude-work", "bfc1769a"),
            ("/tmp/claude-work/", "7c8b7aec"),
            ("./claude-work", "6a30318d"),
            ("~/.claude", "37e4f761"),
            ("/tmp/café", "0873cca0"),
            ("/tmp/cafe\u{301}", "0873cca0"),
        ] {
            let client = client(Some(selector), None);
            assert_eq!(
                client.keychain_service,
                format!("Claude Code-credentials-{suffix}")
            );
            assert_eq!(
                client.credentials_path,
                Path::new(selector).join(".credentials.json")
            );
        }
        for config in [None, Some("")] {
            let client = client(config, None);
            assert_eq!(client.keychain_service, "Claude Code-credentials");
            assert_eq!(
                client.credentials_path,
                Path::new("/Users/test/.claude/.credentials.json")
            );
        }
    }

    #[test]
    fn secure_storage_override_selects_both_the_keychain_and_file() {
        let selected = client(Some("/tmp/ignored"), Some("/tmp/claude-work"));
        assert_eq!(
            selected.keychain_service,
            "Claude Code-credentials-bfc1769a"
        );
        assert_eq!(
            selected.credentials_path,
            Path::new("/tmp/claude-work/.credentials.json")
        );

        let pinned_default = client(Some("/tmp/claude-work"), Some(""));
        assert_eq!(pinned_default.keychain_service, "Claude Code-credentials");
        assert_eq!(
            pinned_default.credentials_path,
            Path::new("/Users/test/.claude/.credentials.json")
        );
    }

    #[test]
    fn credential_reads_stay_within_the_selected_profile() {
        let directory = env::temp_dir().join(format!("codelim-credentials-{}", std::process::id()));
        fs::create_dir(&directory).unwrap();
        let selected = ClaudeClient::for_directories(directory.to_str(), None, None).unwrap();
        let file_token = br#"{"claudeAiOauth":{"accessToken":"file-fixture"}}"#;
        fs::write(&selected.credentials_path, file_token).unwrap();

        let keychain_token = selected
            .read_access_token(|service| {
                assert!(service.starts_with("Claude Code-credentials-"));
                Ok(Some(
                    br#"{"claudeAiOauth":{"accessToken":"keychain-fixture"}}"#.to_vec(),
                ))
            })
            .unwrap();
        assert_eq!(keychain_token, "keychain-fixture");
        assert_eq!(
            selected.read_access_token(|_| Ok(None)).unwrap(),
            "file-fixture"
        );

        // Never mask an invalid/locked selected Keychain with another store.
        assert!(selected
            .read_access_token(|_| Ok(Some(b"invalid".to_vec())))
            .is_err());
        assert!(selected
            .read_access_token(|_| Err(cli_error("locked")))
            .is_err());
        assert_eq!(fs::read(&selected.credentials_path).unwrap(), file_token);

        // Re-read renewed credentials, rather than caching an expired token.
        fs::write(
            &selected.credentials_path,
            br#"{"claudeAiOauth":{"accessToken":"renewed-fixture"}}"#,
        )
        .unwrap();
        assert_eq!(
            selected.read_access_token(|_| Ok(None)).unwrap(),
            "renewed-fixture"
        );
        fs::remove_file(&selected.credentials_path).unwrap();
        assert!(selected
            .read_access_token(|_| Ok(None))
            .unwrap_err()
            .to_string()
            .contains("selected directory"));
        fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn token_expiry_uses_milliseconds_and_rejects_the_exact_boundary() {
        let data = br#"{"claudeAiOauth":{"accessToken":"fixture","expiresAt":1800000000001,"scopes":["user:inference","user:profile"]}}"#;
        assert_eq!(parse_access_token(data, 1800000000000).unwrap(), "fixture");
        assert!(parse_access_token(data, 1800000000001)
            .unwrap_err()
            .to_string()
            .contains("expired"));
        assert!(parse_access_token(data, 1800000000002).is_err());
    }

    #[test]
    fn rejects_api_keys_missing_scopes_and_header_injection_without_echoing_credentials() {
        for value in [
            json!({"apiKey": "secret-fixture"}),
            json!({"claudeAiOauth": {"accessToken": "secret-fixture", "scopes": ["user:inference"]}}),
            json!({"claudeAiOauth": {"accessToken": "secret-fixture\r\nX-Injected: yes"}}),
            json!({"claudeAiOauth": {"accessToken": ""}}),
            json!({"claudeAiOauth": {"accessToken": "secret-fixture", "expiresAt": "secret-fixture"}}),
        ] {
            let bytes = serde_json::to_vec(&value).unwrap();
            let error = parse_access_token(&bytes, 0).unwrap_err().to_string();
            assert!(!error.contains("secret-fixture"));
            assert!(error.contains("/login"));
        }
    }

    #[test]
    fn normalizes_usage_and_allowlists_raw_windows() {
        let body = r#"{
            "five_hour": {"utilization": 12.5, "resets_at": "1970-01-01T02:00:01.123+02:00", "account": "secret-fixture"},
            "seven_day": {"utilization": 83, "resets_at": "1970-01-08T00:00:00Z"},
            "extra_usage": {"used_credits": 42},
            "email": "secret-fixture"
        }"#;
        let result = parse_usage_response("200", body).unwrap();
        assert_eq!(
            serde_json::to_value(&result.snapshot).unwrap(),
            json!({
                "provider": "claude",
                "source": "claude-oauth-api",
                "limits": {
                    "session": {"usedPercent": 12.5, "windowDurationMins": 300, "resetsAt": 1},
                    "weekly": {"usedPercent": 83.0, "windowDurationMins": 10080, "resetsAt": 604800}
                }
            })
        );
        assert_eq!(
            result.raw,
            json!({
                "five_hour": {"utilization": 12.5, "resets_at": "1970-01-01T02:00:01.123+02:00"},
                "seven_day": {"utilization": 83.0, "resets_at": "1970-01-08T00:00:00Z"}
            })
        );
        let text = crate::render_text(&result.snapshot, None, false);
        assert!(text.starts_with("  Claude limits  Claude Code OAuth API\n"));
        assert!(text.contains("87.5% left"));
        assert!(text.contains("17% left"));
        assert!(!text.contains("Codex"));
    }

    #[test]
    fn missing_and_null_windows_are_not_reported_as_unused_quota() {
        for five_hour in ["null", r#"{"utilization":null,"resets_at":null}"#] {
            let body = format!(
                r#"{{"five_hour":{five_hour},"seven_day":{{"utilization":57,"resets_at":null}}}}"#
            );
            let result = parse_usage_response("200", &body).unwrap();
            assert!(result.snapshot.limits.session.is_none());
            assert_eq!(result.snapshot.limits.weekly.unwrap().used_percent, 57.0);
        }
        let result = parse_usage_response("200", "{}").unwrap();
        assert!(result.snapshot.limits.session.is_none());
        assert!(result.snapshot.limits.weekly.is_none());
    }

    #[test]
    fn usage_failures_are_actionable_and_never_echo_server_data() {
        for (status, body, expected) in [
            ("401", "secret-fixture", "/login"),
            ("403", "secret-fixture", "user:profile"),
            ("429", "secret-fixture", "--interval 180"),
            ("503", "secret-fixture", "HTTP 503"),
            (
                "200",
                r#"{"five_hour":{"utilization":"secret-fixture"}}"#,
                "invalid limit data",
            ),
            (
                "200",
                r#"{"five_hour":{"utilization":1,"resets_at":"secret-fixture"}}"#,
                "invalid reset timestamp",
            ),
        ] {
            let error = parse_usage_response(status, body)
                .err()
                .unwrap()
                .to_string();
            assert!(error.contains(expected), "{error}");
            assert!(!error.contains("secret-fixture"));
        }
    }

    #[test]
    fn request_uses_only_the_fixed_https_endpoint_and_stdin_credentials() {
        let command = usage_command();
        assert_eq!(command.get_program(), "/usr/bin/curl");
        let args: Vec<_> = command
            .get_args()
            .map(|arg| arg.to_str().unwrap())
            .collect();
        assert_eq!(args[0], "--disable");
        assert!(args.windows(2).any(|args| args == ["--proto", "=https"]));
        assert!(args.windows(2).any(|args| args == ["--max-time", "10"]));
        assert!(args.windows(2).any(|args| args == ["--header", "@-"]));
        assert_eq!(
            &args[args.len() - 2..],
            ["--url", "https://api.anthropic.com/api/oauth/usage"]
        );
        assert!(!args
            .iter()
            .any(|arg| arg.contains("Authorization") || *arg == "--location"));
    }
}
