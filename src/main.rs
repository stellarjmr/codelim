use chrono::{Local, TimeZone};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::{json, Value};
use std::env;
use std::error::Error;
use std::fmt;
use std::io::{BufRead, BufReader, ErrorKind, IsTerminal, Read, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

const APP_NAME: &str = "codelim";
const APP_VERSION: &str = env!("CARGO_PKG_VERSION");
const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(8);
const RATE_LIMITS_TIMEOUT: Duration = Duration::from_secs(10);

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

    if options.live && (options.json || options.raw) {
        return Err(cli_error("--live cannot be combined with --json or --raw"));
    }
    if options.live && !std::io::stdout().is_terminal() {
        return Err(cli_error("--live requires a TTY on stdout"));
    }
    if options.live && !std::io::stdin().is_terminal() {
        return Err(cli_error(
            "--live requires a TTY on stdin for q/Ctrl-C exit",
        ));
    }

    let mut session = CodexRpcSession::connect(&options.codex_bin, options.verbose)?;

    if options.live {
        return run_live(&mut session, Duration::from_secs(options.interval));
    }

    let raw_limits: Value = session.fetch_rate_limits()?;
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
        print!("{}", render_text(&snapshot, use_color()));
    }

    Ok(())
}

fn run_live(session: &mut CodexRpcSession, interval: Duration) -> Result<()> {
    let color = use_color();
    let _terminal = LiveTerminalMode::enter()?;
    let input_rx = spawn_live_input_reader();
    let mut stdout = std::io::stdout().lock();
    let mut prev_lines = 0usize;
    let mut last_body: Option<String> = None;

    loop {
        let fetch_error = match session
            .fetch_rate_limits()
            .and_then(|raw_limits| Ok(serde_json::from_value::<RateLimitsResponse>(raw_limits)?))
        {
            Ok(limits_response) => {
                let snapshot = Snapshot::from_rpc(limits_response.rate_limits);
                last_body = Some(render_text(&snapshot, color));
                None
            }
            Err(error) => Some(error.to_string()),
        };

        let body = last_body.as_deref().unwrap_or("");
        let error_line = fetch_error
            .as_deref()
            .map(|error| render_live_error(error, color))
            .unwrap_or_default();
        let footer = render_live_footer(interval, color);
        let frame = format!("{body}{error_line}{footer}\n");

        if prev_lines > 0 {
            write!(stdout, "\x1b[{prev_lines}F\x1b[J")?;
        }
        write!(stdout, "{frame}")?;
        stdout.flush()?;

        prev_lines = frame.matches('\n').count();

        if wait_for_live_exit(&input_rx, interval) {
            break;
        }
    }

    Ok(())
}

fn render_live_error(error: &str, color: bool) -> String {
    let now = Local::now().format("%H:%M:%S");
    let message = truncate_chars(&error.replace(['\n', '\r'], " "), 70);
    format!(
        "{}\n",
        paint(
            &format!("  ⚠ {now} fetch failed, retrying: {message}"),
            "31",
            color
        )
    )
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let truncated: String = text.chars().take(max_chars.saturating_sub(1)).collect();
    format!("{truncated}…")
}

fn render_live_footer(interval: Duration, color: bool) -> String {
    let now = Local::now().format("%H:%M:%S");
    let secs = interval.as_secs().max(1);
    paint(
        &format!("  updated {now} · every {secs}s · q/Ctrl-C to exit"),
        "2",
        color,
    )
}

fn spawn_live_input_reader() -> Receiver<()> {
    let (tx, rx) = mpsc::channel();

    thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        let mut byte = [0u8; 1];

        loop {
            match stdin.read(&mut byte) {
                Ok(0) => break,
                Ok(_) if matches!(byte[0], b'q' | b'Q' | 0x03) => {
                    let _ = tx.send(());
                    break;
                }
                Ok(_) => {}
                Err(error) if error.kind() == ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
    });

    rx
}

fn wait_for_live_exit(input_rx: &Receiver<()>, interval: Duration) -> bool {
    match input_rx.recv_timeout(interval) {
        Ok(()) => true,
        Err(RecvTimeoutError::Timeout) => false,
        Err(RecvTimeoutError::Disconnected) => {
            thread::sleep(interval);
            false
        }
    }
}

struct LiveTerminalMode {
    fd: libc::c_int,
    original: libc::termios,
}

impl LiveTerminalMode {
    fn enter() -> Result<Self> {
        let fd = libc::STDIN_FILENO;

        // Use non-canonical input so a single `q` keypress can stop live mode
        // without waiting for Enter. Disable terminal-generated signals too so
        // Ctrl-C exits through the same cleanup path and restores the TTY mode.
        unsafe {
            let mut original = std::mem::MaybeUninit::<libc::termios>::uninit();
            if libc::tcgetattr(fd, original.as_mut_ptr()) != 0 {
                return Err(cli_error(format!(
                    "failed to read terminal input mode: {}",
                    std::io::Error::last_os_error()
                )));
            }

            let original = original.assume_init();
            let mut live = original;
            live.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ISIG);
            live.c_cc[libc::VMIN] = 1;
            live.c_cc[libc::VTIME] = 0;

            if libc::tcsetattr(fd, libc::TCSANOW, &live) != 0 {
                return Err(cli_error(format!(
                    "failed to configure terminal input mode: {}",
                    std::io::Error::last_os_error()
                )));
            }

            Ok(Self { fd, original })
        }
    }
}

impl Drop for LiveTerminalMode {
    fn drop(&mut self) {
        unsafe {
            let _ = libc::tcsetattr(self.fd, libc::TCSANOW, &self.original);
        }
    }
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
struct RpcTransportError(String);

impl fmt::Display for RpcTransportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Error for RpcTransportError {}

fn rpc_transport_error(message: impl Into<String>) -> Box<dyn Error + Send + Sync> {
    Box::new(RpcTransportError(message.into()))
}

fn is_rpc_transport_error(error: &(dyn Error + Send + Sync + 'static)) -> bool {
    error.downcast_ref::<RpcTransportError>().is_some()
}

#[derive(Debug)]
struct Options {
    codex_bin: String,
    json: bool,
    raw: bool,
    live: bool,
    interval: u64,
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
            live: false,
            interval: 10,
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
                "--live" => options.live = true,
                "--interval" => {
                    let value = args
                        .next()
                        .ok_or_else(|| cli_error("--interval requires a number of seconds"))?;
                    let secs: u64 = value.parse().map_err(|_| {
                        cli_error(format!("--interval expects an integer, got `{value}`"))
                    })?;
                    if secs == 0 {
                        return Err(cli_error("--interval must be at least 1 second"));
                    }
                    options.interval = secs;
                }
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
OPTIONS:\n    --json              Print normalized JSON\n    --raw               Print raw Codex limit windows\n    --live              Continuously refresh in-place; press q to exit (requires a TTY)\n    --interval <SECS>   Refresh interval for --live (default: 10)\n    --codex-bin <PATH>  Codex executable path (default: codex)\n    -v, --verbose       Print Codex app-server stderr\n    -h, --help          Print help\n    -V, --version       Print version\n\n\
It starts: codex -s read-only app-server\n\
and reads account/rateLimits/read from the local Codex CLI session."
    );
}

struct CodexRpcSession {
    codex_bin: String,
    verbose: bool,
    client: CodexRpcClient,
}

impl CodexRpcSession {
    fn connect(codex_bin: &str, verbose: bool) -> Result<Self> {
        let client = Self::start_client(codex_bin, verbose)?;
        Ok(Self {
            codex_bin: codex_bin.to_string(),
            verbose,
            client,
        })
    }

    fn start_client(codex_bin: &str, verbose: bool) -> Result<CodexRpcClient> {
        let mut client = CodexRpcClient::spawn(codex_bin, verbose)?;
        let _: Value = client.request(
            "initialize",
            json!({
                "clientInfo": {
                    "name": APP_NAME,
                    "version": APP_VERSION,
                }
            }),
            INITIALIZE_TIMEOUT,
        )?;
        client.notify("initialized", json!({}))?;
        Ok(client)
    }

    fn fetch_rate_limits(&mut self) -> Result<Value> {
        match self.fetch_rate_limits_once() {
            Ok(rate_limits) => Ok(rate_limits),
            Err(error) if is_rpc_transport_error(error.as_ref()) => {
                if self.verbose {
                    eprintln!("[codelim] {error}; restarting Codex app-server");
                }

                self.client = Self::start_client(&self.codex_bin, self.verbose)?;
                self.fetch_rate_limits_once()
            }
            Err(error) => Err(error),
        }
    }

    fn fetch_rate_limits_once(&mut self) -> Result<Value> {
        self.client
            .request("account/rateLimits/read", json!({}), RATE_LIMITS_TIMEOUT)
    }
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
            .args(["-s", "read-only", "app-server"])
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
                        let message = serde_json::from_str::<Value>(trimmed).map_err(|error| {
                            format!("invalid JSON from Codex: {error}: {trimmed}")
                        });
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
                return Err(rpc_transport_error(format!(
                    "Codex RPC timed out waiting for `{method}`"
                )));
            }

            let remaining = deadline.saturating_duration_since(now);
            let message = match self.rx.recv_timeout(remaining) {
                Ok(message) => message.map_err(rpc_transport_error)?,
                Err(RecvTimeoutError::Timeout) => {
                    return Err(rpc_transport_error(format!(
                        "Codex RPC timed out waiting for `{method}`"
                    )));
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(rpc_transport_error(format!(
                        "Codex app-server closed before `{method}` replied"
                    )));
                }
            };

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
        serde_json::to_writer(&mut self.stdin, &payload).map_err(|error| {
            rpc_transport_error(format!("failed writing to Codex app-server: {error}"))
        })?;
        self.stdin.write_all(b"\n").map_err(|error| {
            rpc_transport_error(format!("failed writing to Codex app-server: {error}"))
        })?;
        self.stdin.flush().map_err(|error| {
            rpc_transport_error(format!("failed writing to Codex app-server: {error}"))
        })?;
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

        let session = take_window(&mut windows, WindowRole::Session);
        let weekly = take_window(&mut windows, WindowRole::Weekly);
        let session = session.or_else(|| take_first(&mut windows));
        let weekly = weekly.or_else(|| take_first(&mut windows));

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

fn render_text(snapshot: &Snapshot, color: bool) -> String {
    use std::fmt::Write as _;

    let mut out = String::new();
    let rule = "──────────────────────────────────────────";

    let _ = writeln!(
        out,
        "  {}  {}",
        paint("Codex limits", "1;36", color),
        paint("local Codex CLI RPC", "2", color),
    );
    let _ = writeln!(out, "  {}", paint(rule, "2", color));
    render_section(&mut out, "5-hour", snapshot.limits.session.as_ref(), color);
    render_section(&mut out, "Weekly", snapshot.limits.weekly.as_ref(), color);
    out
}

fn render_section(out: &mut String, label: &str, window: Option<&RateWindow>, color: bool) {
    use std::fmt::Write as _;

    let label_styled = paint(&format!("{label:<7}"), "1", color);

    let Some(window) = window else {
        let _ = writeln!(
            out,
            "  {label_styled} {}",
            paint("not available", "2", color)
        );
        return;
    };

    let remaining = (100.0 - window.used_percent).clamp(0.0, 100.0);
    let bar = usage_bar(remaining, 20);
    let bar_styled = paint(&bar, bar_color_code(remaining), color);
    let pct_styled = paint(&format!("{} left", format_percent(remaining)), "1", color);

    let _ = writeln!(out, "  {label_styled} {bar_styled}  {pct_styled}");

    if let Some(resets_at) = window.resets_at {
        let _ = writeln!(
            out,
            "          {} {}",
            paint("↻ Resets", "2", color),
            paint(&format_reset(resets_at), "2", color),
        );
    }
}

fn use_color() -> bool {
    std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none()
}

fn paint(text: &str, code: &str, enabled: bool) -> String {
    if enabled {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

fn bar_color_code(remaining: f64) -> &'static str {
    if remaining >= 50.0 {
        "32"
    } else if remaining >= 20.0 {
        "33"
    } else {
        "31"
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
    format!("{}{}", "▰".repeat(filled), "▱".repeat(width - filled))
}

fn format_reset(timestamp: i64) -> String {
    let now = Local::now().timestamp();
    let delta = timestamp.saturating_sub(now);
    let absolute = Local
        .timestamp_opt(timestamp, 0)
        .single()
        .map(|time| time.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| timestamp.to_string());

    if delta <= 0 {
        format!("now · {absolute}")
    } else {
        format!("in {} · {absolute}", human_duration(delta))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn window(duration_mins: Option<i64>) -> RateWindow {
        RateWindow {
            used_percent: 25.0,
            window_duration_mins: duration_mins,
            resets_at: None,
        }
    }

    fn snapshot(primary: Option<RateWindow>, secondary: Option<RateWindow>) -> Snapshot {
        Snapshot::from_rpc(RateLimitSnapshot { primary, secondary })
    }

    fn duration(window: &Option<RateWindow>) -> Option<i64> {
        window
            .as_ref()
            .and_then(|window| window.window_duration_mins)
    }

    #[test]
    fn keeps_a_weekly_only_window_in_the_weekly_slot() {
        let snapshot = snapshot(Some(window(Some(10080))), None);

        assert!(snapshot.limits.session.is_none());
        assert_eq!(duration(&snapshot.limits.weekly), Some(10080));
    }

    #[test]
    fn classifies_known_windows_before_falling_back_to_unknown_windows() {
        let snapshot = snapshot(Some(window(Some(10080))), Some(window(None)));

        assert!(snapshot.limits.session.is_some());
        assert_eq!(duration(&snapshot.limits.session), None);
        assert_eq!(duration(&snapshot.limits.weekly), Some(10080));
    }

    #[test]
    fn classifies_known_windows_regardless_of_rpc_order() {
        let snapshot = snapshot(Some(window(Some(10080))), Some(window(Some(300))));

        assert_eq!(duration(&snapshot.limits.session), Some(300));
        assert_eq!(duration(&snapshot.limits.weekly), Some(10080));
    }
}
