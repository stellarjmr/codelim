use chrono::{Local, TimeZone};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::{json, Value};
use std::env;
use std::error::Error;
use std::fmt;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

const APP_NAME: &str = "codelim";
const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() {
    if let Err(error) = run() {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let options = Options::parse(env::args().skip(1))?;

    if options.help {
        print_help();
        return Ok(());
    }
    if options.version {
        println!("{APP_NAME} {APP_VERSION}");
        return Ok(());
    }

    let mut client = CodexRpcClient::spawn(&options.codex_bin, options.verbose)?;

    let _: Value = client.request(
        "initialize",
        json!({
            "clientInfo": {
                "name": APP_NAME,
                "version": APP_VERSION,
            }
        }),
        Duration::from_secs(8),
    )?;
    client.notify("initialized", json!({}))?;

    let raw_limits: Value = client.request(
        "account/rateLimits/read",
        json!({}),
        Duration::from_secs(3),
    )?;
    let limits_response: RateLimitsResponse = serde_json::from_value(raw_limits.clone())?;

    if options.raw {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "primary": &limits_response.rate_limits.primary,
                "secondary": &limits_response.rate_limits.secondary,
            }))?
        );
        return Ok(());
    }

    let snapshot = Snapshot::from_rpc(limits_response.rate_limits);

    if options.json {
        println!("{}", serde_json::to_string_pretty(&snapshot)?);
    } else {
        print_text(&snapshot);
    }

    Ok(())
}

type Result<T> = std::result::Result<T, Box<dyn Error + Send + Sync>>;

#[derive(Debug)]
struct CliError(String);

impl fmt::Display for CliError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Error for CliError {}

fn cli_error(message: impl Into<String>) -> Box<dyn Error + Send + Sync> {
    Box::new(CliError(message.into()))
}

#[derive(Debug)]
struct Options {
    codex_bin: String,
    json: bool,
    raw: bool,
    verbose: bool,
    help: bool,
    version: bool,
}

impl Options {
    fn parse(args: impl Iterator<Item = String>) -> Result<Self> {
        let mut options = Options {
            codex_bin: env::var("CODELIM_CODEX_BIN")
                .or_else(|_| env::var("CODEX_BIN"))
                .unwrap_or_else(|_| "codex".to_string()),
            json: false,
            raw: false,
            verbose: false,
            help: false,
            version: false,
        };

        let mut args = args.peekable();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "-h" | "--help" => options.help = true,
                "-V" | "--version" => options.version = true,
                "--json" => options.json = true,
                "--raw" => options.raw = true,
                "-v" | "--verbose" => options.verbose = true,
                "--codex-bin" => {
                    options.codex_bin = args
                        .next()
                        .ok_or_else(|| cli_error("--codex-bin requires a path"))?;
                }
                other => return Err(cli_error(format!("unknown argument: {other}"))),
            }
        }

        Ok(options)
    }
}

fn print_help() {
    println!(
        "{APP_NAME} {APP_VERSION}\n\n\
Minimal local Codex quota checker.\n\n\
USAGE:\n    codelim [OPTIONS]\n\n\
OPTIONS:\n    --json              Print normalized JSON\n    --raw               Print raw Codex limit windows\n    --codex-bin <PATH>  Codex executable path (default: codex)\n    -v, --verbose       Print Codex app-server stderr\n    -h, --help          Print help\n    -V, --version       Print version\n\n\
It starts: codex -s read-only -a untrusted app-server\n\
and reads account/rateLimits/read from the local Codex CLI session."
    );
}

struct CodexRpcClient {
    child: Child,
    stdin: ChildStdin,
    rx: Receiver<std::result::Result<Value, String>>,
    next_id: u64,
}

impl CodexRpcClient {
    fn spawn(codex_bin: &str, verbose: bool) -> Result<Self> {
        let mut child = Command::new(codex_bin)
            .args(["-s", "read-only", "-a", "untrusted", "app-server"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| {
                cli_error(format!(
                    "failed to start `{codex_bin}`. Is Codex CLI installed and on PATH? ({error})"
                ))
            })?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| cli_error("failed to open Codex stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| cli_error("failed to open Codex stdout"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| cli_error("failed to open Codex stderr"))?;

        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                match line {
                    Ok(line) => {
                        let trimmed = line.trim();
                        if trimmed.is_empty() {
                            continue;
                        }
                        let message = serde_json::from_str::<Value>(trimmed)
                            .map_err(|error| format!("invalid JSON from Codex: {error}: {trimmed}"));
                        if tx.send(message).is_err() {
                            break;
                        }
                    }
                    Err(error) => {
                        let _ = tx.send(Err(format!("failed reading Codex stdout: {error}")));
                        break;
                    }
                }
            }
        });

        thread::spawn(move || {
            if verbose {
                let reader = BufReader::new(stderr);
                for line in reader.lines().map_while(std::result::Result::ok) {
                    eprintln!("[codex] {line}");
                }
            } else {
                let mut stderr = stderr;
                let mut sink = Vec::new();
                let _ = stderr.read_to_end(&mut sink);
            }
        });

        Ok(Self {
            child,
            stdin,
            rx,
            next_id: 1,
        })
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        self.send(json!({
            "method": method,
            "params": params,
        }))
    }

    fn request<T: DeserializeOwned>(
        &mut self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<T> {
        let id = self.next_id;
        self.next_id += 1;

        self.send(json!({
            "id": id,
            "method": method,
            "params": params,
        }))?;

        let deadline = Instant::now() + timeout;
        loop {
            let now = Instant::now();
            if now >= deadline {
                return Err(cli_error(format!(
                    "Codex RPC timed out waiting for `{method}`"
                )));
            }

            let remaining = deadline.saturating_duration_since(now);
            let message = self
                .rx
                .recv_timeout(remaining)
                .map_err(|_| cli_error(format!("Codex app-server closed before `{method}` replied")))?
                .map_err(cli_error)?;

            if message.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }

            if let Some(error) = message.get("error") {
                let text = error
                    .get("message")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| error.to_string());
                return Err(cli_error(format!("Codex RPC `{method}` failed: {text}")));
            }

            let result = message
                .get("result")
                .cloned()
                .ok_or_else(|| cli_error(format!("Codex RPC `{method}` returned no result")))?;
            return Ok(serde_json::from_value(result)?);
        }
    }

    fn send(&mut self, payload: Value) -> Result<()> {
        serde_json::to_writer(&mut self.stdin, &payload)?;
        self.stdin.write_all(b"\n")?;
        self.stdin.flush()?;
        Ok(())
    }
}

impl Drop for CodexRpcClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[derive(Debug, Deserialize)]
struct RateLimitsResponse {
    #[serde(rename = "rateLimits")]
    rate_limits: RateLimitSnapshot,
}

#[derive(Debug, Deserialize)]
struct RateLimitSnapshot {
    primary: Option<RateWindow>,
    secondary: Option<RateWindow>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct RateWindow {
    #[serde(rename = "usedPercent")]
    used_percent: f64,
    #[serde(rename = "windowDurationMins")]
    window_duration_mins: Option<i64>,
    #[serde(rename = "resetsAt")]
    resets_at: Option<i64>,
}

#[derive(Debug, Serialize)]
struct Snapshot {
    provider: &'static str,
    source: &'static str,
    limits: LimitSummary,
}

#[derive(Debug, Serialize)]
struct LimitSummary {
    session: Option<RateWindow>,
    weekly: Option<RateWindow>,
}

impl Snapshot {
    fn from_rpc(rate_limits: RateLimitSnapshot) -> Self {
        let mut windows = vec![rate_limits.primary, rate_limits.secondary]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();

        let session = take_window(&mut windows, WindowRole::Session).or_else(|| take_first(&mut windows));
        let weekly = take_window(&mut windows, WindowRole::Weekly).or_else(|| take_first(&mut windows));

        Self {
            provider: "codex",
            source: "codex-cli-rpc",
            limits: LimitSummary { session, weekly },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WindowRole {
    Session,
    Weekly,
    Unknown,
}

fn role(window: &RateWindow) -> WindowRole {
    match window.window_duration_mins {
        Some(300) => WindowRole::Session,
        Some(10080) => WindowRole::Weekly,
        _ => WindowRole::Unknown,
    }
}

fn take_window(windows: &mut Vec<RateWindow>, wanted: WindowRole) -> Option<RateWindow> {
    let index = windows.iter().position(|window| role(window) == wanted)?;
    Some(windows.remove(index))
}

fn take_first(windows: &mut Vec<RateWindow>) -> Option<RateWindow> {
    if windows.is_empty() {
        None
    } else {
        Some(windows.remove(0))
    }
}

fn print_text(snapshot: &Snapshot) {
    println!("Codex limits (local CLI RPC)");

    match &snapshot.limits.session {
        Some(window) => print_window("5-hour", window),
        None => println!("5-hour: not available"),
    }

    match &snapshot.limits.weekly {
        Some(window) => print_window("Weekly", window),
        None => println!("Weekly: not available"),
    }

}

fn print_window(label: &str, window: &RateWindow) {
    let remaining = (100.0 - window.used_percent).clamp(0.0, 100.0);
    println!(
        "{label}: {} remaining ({} used) {}",
        format_percent(remaining),
        format_percent(window.used_percent),
        usage_bar(remaining, 20)
    );

    if let Some(resets_at) = window.resets_at {
        println!("  Reset: {}", format_reset(resets_at));
    }
}

fn format_percent(value: f64) -> String {
    if (value.fract()).abs() < 0.05 {
        format!("{value:.0}%")
    } else {
        format!("{value:.1}%")
    }
}

fn usage_bar(remaining_percent: f64, width: usize) -> String {
    let filled = ((remaining_percent / 100.0) * width as f64).round() as usize;
    let filled = filled.min(width);
    format!("[{}{}]", "#".repeat(filled), "-".repeat(width - filled))
}

fn format_reset(timestamp: i64) -> String {
    let now = Local::now().timestamp();
    let delta = timestamp.saturating_sub(now);
    let absolute = Local
        .timestamp_opt(timestamp, 0)
        .single()
        .map(|time| time.format("%Y-%m-%d %H:%M:%S %Z").to_string())
        .unwrap_or_else(|| timestamp.to_string());

    if delta == 0 {
        format!("now ({absolute})")
    } else {
        format!("in {} ({absolute})", human_duration(delta))
    }
}

fn human_duration(seconds: i64) -> String {
    let minutes = (seconds + 59) / 60;
    let days = minutes / (60 * 24);
    let hours = (minutes % (60 * 24)) / 60;
    let mins = minutes % 60;

    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {mins}m")
    } else {
        format!("{mins}m")
    }
}

