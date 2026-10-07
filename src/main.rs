mod claude;

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

    if !options.claude_config_dirs.is_empty() && options.provider == Provider::Codex {
        return Err(cli_error(
            "--claude-config-dir cannot be combined with --provider codex",
        ));
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

    let mut entries = Vec::new();
    if options.provider != Provider::Claude {
        entries.push(LimitEntry::new(
            LimitClient::Codex {
                codex_bin: options.codex_bin.clone(),
                verbose: options.verbose,
                session: None,
            },
            options.interval,
        ));
    }
    if options.provider != Provider::Codex {
        for client in claude::ClaudeClient::discover(&options.claude_config_dirs)? {
            entries.push(LimitEntry::new(
                LimitClient::Claude(client),
                options.interval,
            ));
        }
    }

    if options.live {
        let cadence = match (options.interval, options.provider) {
            (Some(seconds), _) => format!("every {seconds}s"),
            (None, Provider::All) => "Codex 10s · Claude 180s".to_string(),
            (None, Provider::Codex) => "every 10s".to_string(),
            (None, Provider::Claude) => "every 180s".to_string(),
        };
        return run_live(&mut entries, &cadence);
    }

    for entry in &mut entries {
        entry.refresh_if_due();
    }
    // Keep the original single-Codex output contract when explicitly selected.
    if options.provider == Provider::Codex {
        if let Some(error) = &entries[0].error {
            return Err(cli_error(error.clone()));
        }
    }

    if options.raw || options.json {
        let mut results = entries
            .iter()
            .map(|entry| entry.json(options.raw))
            .collect::<Result<Vec<_>>>()?;
        let output = if options.provider == Provider::Codex {
            let mut result = results.remove(0);
            if options.raw {
                result["windows"].take()
            } else {
                result
            }
        } else {
            json!({"results": results})
        };
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        print!("{}", render_entries(&entries, use_color(), false));
    }
    if entries.iter().any(|entry| entry.error.is_some()) {
        return Err(cli_error(
            "some limit reads failed; other accounts are shown above",
        ));
    }

    Ok(())
}

struct LimitRead {
    snapshot: Snapshot,
    raw: Value,
}

enum LimitClient {
    Codex {
        codex_bin: String,
        verbose: bool,
        session: Option<CodexRpcSession>,
    },
    Claude(claude::ClaudeClient),
}

impl LimitClient {
    fn provider(&self) -> &'static str {
        match self {
            Self::Codex { .. } => "codex",
            Self::Claude(_) => "claude",
        }
    }

    fn profile(&self) -> Option<&str> {
        match self {
            Self::Codex { .. } => None,
            Self::Claude(client) => Some(&client.profile),
        }
    }

    fn fetch(&mut self) -> Result<LimitRead> {
        match self {
            Self::Codex {
                codex_bin,
                verbose,
                session,
            } => {
                let session = match session {
                    Some(session) => session,
                    None => session.insert(CodexRpcSession::connect(codex_bin, *verbose)?),
                };
                let response: RateLimitsResponse =
                    serde_json::from_value(session.fetch_rate_limits()?)?;
                let raw = json!({
                    "primary": &response.rate_limits.primary,
                    "secondary": &response.rate_limits.secondary,
                });
                Ok(LimitRead {
                    snapshot: Snapshot::from_rpc(response.rate_limits),
                    raw,
                })
            }
            Self::Claude(client) => client.fetch(),
        }
    }
}

struct LimitEntry {
    client: LimitClient,
    latest: Option<LimitRead>,
    error: Option<String>,
    interval: Duration,
    next_refresh: Instant,
}

impl LimitEntry {
    fn new(client: LimitClient, interval: Option<u64>) -> Self {
        let seconds = interval.unwrap_or(if client.provider() == "codex" {
            10
        } else {
            180
        });
        Self {
            client,
            latest: None,
            error: None,
            interval: Duration::from_secs(seconds),
            next_refresh: Instant::now(),
        }
    }

    fn refresh_if_due(&mut self) {
        if Instant::now() < self.next_refresh {
            return;
        }
        let result = self.client.fetch();
        self.record(result);
        self.next_refresh = Instant::now() + self.interval;
    }

    fn record(&mut self, result: Result<LimitRead>) {
        match result {
            Ok(limits) => {
                self.latest = Some(limits);
                self.error = None;
            }
            Err(error) => self.error = Some(error.to_string()),
        }
    }

    fn json(&self, raw: bool) -> Result<Value> {
        let mut value = match &self.latest {
            Some(limits) if raw => {
                json!({"provider": self.client.provider(), "windows": limits.raw})
            }
            Some(limits) => serde_json::to_value(&limits.snapshot)?,
            None => json!({"provider": self.client.provider()}),
        };
        if let Some(profile) = self.client.profile() {
            value["profile"] = json!(profile);
        }
        if let Some(error) = &self.error {
            value["error"] = json!(error);
        }
        Ok(value)
    }
}

fn render_entries(entries: &[LimitEntry], color: bool, live: bool) -> String {
    let mut output = String::new();
    for (index, entry) in entries.iter().enumerate() {
        if index > 0 {
            output.push('\n');
        }
        output.push_str(&match &entry.latest {
            Some(limits) => render_text(&limits.snapshot, entry.client.profile(), color),
            None => render_header(entry.client.provider(), entry.client.profile(), color),
        });
        if let Some(error) = &entry.error {
            if live {
                output.push_str(&render_live_error(error, color));
            } else {
                output.push_str(&format!(
                    "  {}\n",
                    paint(&format!("⚠ {error}"), "31", color)
                ));
            }
        }
    }
    output
}

fn run_live(entries: &mut [LimitEntry], cadence: &str) -> Result<()> {
    let color = use_color();
    let _terminal = LiveTerminalMode::enter()?;
    let input_rx = spawn_live_input_reader();
    let mut stdout = std::io::stdout().lock();
    let mut prev_lines = 0usize;

    loop {
        for entry in entries.iter_mut() {
            if input_rx.try_recv().is_ok() {
                return Ok(());
            }
            entry.refresh_if_due();
        }

        let body = render_entries(entries, color, true);
        let footer = render_live_footer(cadence, color);
        let frame = format!("{body}{footer}\n");

        if prev_lines > 0 {
            write!(stdout, "\x1b[{prev_lines}F\x1b[J")?;
        }
        write!(stdout, "{frame}")?;
        stdout.flush()?;

        prev_lines = frame.matches('\n').count();

        let wait = entries
            .iter()
            .map(|entry| entry.next_refresh.saturating_duration_since(Instant::now()))
            .min()
            .unwrap_or(Duration::from_secs(1));
        if wait_for_live_exit(&input_rx, wait) {
            break;
        }
    }

    Ok(())
}

fn render_live_error(error: &str, color: bool) -> String {
    let now = Local::now().format("%H:%M:%S");
    let message = truncate_chars(
        &format!(
            "  ⚠ {now} fetch failed, retrying: {}",
            error.replace(['\n', '\r'], " ")
        ),
        78,
    );
    format!("{}\n", paint(&message, "31", color))
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let truncated: String = text.chars().take(max_chars.saturating_sub(1)).collect();
    format!("{truncated}…")
}

fn render_live_footer(cadence: &str, color: bool) -> String {
    let now = Local::now().format("%H:%M:%S");
    paint(
        &format!("  updated {now} · {cadence} · q/Ctrl-C to exit"),
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Provider {
    All,
    Codex,
    Claude,
}

#[derive(Debug)]
struct Options {
    provider: Provider,
    codex_bin: String,
    claude_config_dirs: Vec<String>,
    json: bool,
    raw: bool,
    live: bool,
    interval: Option<u64>,
    verbose: bool,
    help: bool,
    version: bool,
}

impl Options {
    fn parse(args: impl Iterator<Item = String>) -> Result<Self> {
        let mut options = Options {
            provider: Provider::All,
            codex_bin: env::var("CODELIM_CODEX_BIN")
                .or_else(|_| env::var("CODEX_BIN"))
                .unwrap_or_else(|_| "codex".to_string()),
            claude_config_dirs: Vec::new(),
            json: false,
            raw: false,
            live: false,
            interval: None,
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
                "--provider" => {
                    options.provider = match args.next().as_deref() {
                        Some("all") => Provider::All,
                        Some("codex") => Provider::Codex,
                        Some("claude") => Provider::Claude,
                        _ => return Err(cli_error("--provider expects all, codex, or claude")),
                    };
                }
                "--claude-config-dir" => {
                    options.claude_config_dirs.push(
                        args.next()
                            .filter(|path| !path.is_empty() && !path.starts_with('-'))
                            .ok_or_else(|| cli_error("--claude-config-dir requires a path"))?,
                    );
                }
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
                    options.interval = Some(secs);
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
Minimal Codex and Claude Code quota checker.\n\n\
USAGE:\n    codelim [OPTIONS]\n\n\
OPTIONS:\n    --provider <NAME>         all (default), codex, or claude\n    --json                    Print normalized JSON results for all accounts\n    --raw                     Print raw limit windows for all accounts\n    --live                    Refresh in-place; q/Ctrl-C to exit (TTY only)\n    --interval <SECS>         Override refresh cadence for every account\n    --codex-bin <PATH>        Codex executable path (default: codex)\n    --claude-config-dir <DIR> Add a Claude account directory (repeatable)\n    -v, --verbose             Print Codex app-server stderr\n    -h, --help                Print help\n    -V, --version             Print version\n\n\
Shows Codex and all discovered Claude accounts together by default.\n\
Claude discovery: default login, ~/.claude[-_]* directories, environment\n\
CLAUDE_CONFIG_DIR / CLAUDE_SECURESTORAGE_CONFIG_DIR, and explicit directories.\n\
Each account refreshes independently: Codex every 10s, Claude every 180s.\n\
Codex: starts codex -s read-only app-server and reads account/rateLimits/read.\n\
Claude: reads existing Claude Code credentials and queries the OAuth usage API.\n\
Credentials are read-only; no automatic login or token refresh.\n\n\
EXAMPLES:\n    codelim\n    codelim --live\n    codelim --provider claude --json\n    codelim --claude-config-dir /accounts/work --claude-config-dir /accounts/personal"
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

fn render_text(snapshot: &Snapshot, profile: Option<&str>, color: bool) -> String {
    let mut out = render_header(snapshot.provider, profile, color);
    render_section(&mut out, "5-hour", snapshot.limits.session.as_ref(), color);
    render_section(&mut out, "Weekly", snapshot.limits.weekly.as_ref(), color);
    out
}

fn render_header(provider: &str, profile: Option<&str>, color: bool) -> String {
    use std::fmt::Write as _;

    let mut out = String::new();
    let rule = "──────────────────────────────────────────";
    let (title, source) = match provider {
        "claude" => ("Claude limits", "Claude Code OAuth API"),
        _ => ("Codex limits", "local Codex CLI RPC"),
    };
    let title = match profile {
        Some(profile) => format!(
            "{title} [{}]",
            truncate_chars(
                &profile
                    .chars()
                    .filter(|c| !c.is_control())
                    .collect::<String>(),
                32
            )
        ),
        None => title.to_string(),
    };

    let _ = writeln!(
        out,
        "  {}  {}",
        paint(&title, "1;36", color),
        paint(source, "2", color),
    );
    let _ = writeln!(out, "  {}", paint(rule, "2", color));
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
            paint("Resets", "2", color),
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
    format!("{}{}", "━".repeat(filled), "┄".repeat(width - filled))
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

    #[test]
    fn defaults_to_all_and_accepts_multiple_claude_directories() {
        let defaults = Options::parse(std::iter::empty()).unwrap();
        assert_eq!(defaults.provider, Provider::All);
        assert_eq!(defaults.interval, None);
        assert!(defaults.claude_config_dirs.is_empty());

        let options = Options::parse(
            [
                "--provider",
                "claude",
                "--claude-config-dir",
                "/tmp/claude work/",
                "--claude-config-dir",
                "/tmp/claude-personal",
                "--live",
                "--interval",
                "240",
            ]
            .into_iter()
            .map(str::to_string),
        )
        .unwrap();
        assert_eq!(options.provider, Provider::Claude);
        assert_eq!(
            options.claude_config_dirs,
            ["/tmp/claude work/", "/tmp/claude-personal"]
        );
        assert!(options.live);
        assert_eq!(options.interval, Some(240));
    }

    #[test]
    fn rejects_invalid_provider_and_profile_arguments() {
        for args in [
            vec!["--provider"],
            vec!["--provider", "unknown"],
            vec!["--claude-config-dir"],
            vec!["--claude-config-dir", "--json"],
            vec!["--claude-config-dir", ""],
            vec!["--provider", "claude", "--interval", "0"],
        ] {
            assert!(Options::parse(args.into_iter().map(str::to_string)).is_err());
        }
    }

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

    #[test]
    fn renders_twenty_cell_line_bars() {
        for (remaining, expected) in [
            (0.0, "┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄"),
            (43.0, "━━━━━━━━━┄┄┄┄┄┄┄┄┄┄┄"),
            (100.0, "━━━━━━━━━━━━━━━━━━━━"),
        ] {
            assert_eq!(usage_bar(remaining, 20), expected);
        }
    }

    #[test]
    fn renders_remaining_quota_with_a_plain_reset_label() {
        let snapshot = snapshot(
            None,
            Some(RateWindow {
                used_percent: 57.0,
                window_duration_mins: Some(10080),
                resets_at: Some(0),
            }),
        );

        for color in [false, true] {
            let text = render_text(&snapshot, None, color);
            assert!(text.contains("━━━━━━━━━┄┄┄┄┄┄┄┄┄┄┄"));
            assert!(text.contains("43% left"));
            let reset_prefix = if color {
                "          \x1b[2mResets\x1b[0m \x1b[2mnow · "
            } else {
                "          Resets now · "
            };
            assert!(text.contains(reset_prefix));
            assert!(!text.contains('↻'));
        }
    }
}
