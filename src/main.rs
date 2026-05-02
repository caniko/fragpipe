use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::Read;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, Subcommand, ValueEnum};
use regex::Regex;
use serde::{Deserialize, Serialize};

#[derive(Debug, Parser)]
#[command(name = "fragpipe")]
#[command(about = "Bare-metal multiplayer test orchestration")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Run a native WebRTC Direct 1v1 smoke test.
    #[command(name = "webrtc-1v1")]
    Webrtc1v1(WebRtc1v1Args),
}

#[derive(Debug, Parser)]
struct WebRtc1v1Args {
    /// Project config path.
    #[arg(long, default_value = "fragpipe.toml")]
    config: PathBuf,

    /// Number of test runs.
    #[arg(long)]
    max_runs: Option<u32>,

    /// No-progress timeout per run in seconds.
    #[arg(long)]
    timeout: Option<u64>,

    /// Stop after the first failed run.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    stop_on_failure: bool,

    /// Skip the configured build command.
    #[arg(long)]
    no_build: bool,

    /// Skip binary/assets deployment.
    #[arg(long)]
    no_deploy: bool,

    /// Remote peer name from the config.
    #[arg(long)]
    remote: Option<String>,

    /// Reachable local IP used when the WebRTC listen address is wildcard or loopback.
    #[arg(long)]
    local_ip: Option<IpAddr>,

    /// Local WebRTC listen port.
    #[arg(long)]
    webrtc_port: Option<u16>,

    /// Print commands without launching or SSHing.
    #[arg(long)]
    dry_run: bool,

    /// Output format.
    #[arg(long, default_value = "text")]
    output_format: OutputFormat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum OutputFormat {
    Text,
    Jsonl,
}

#[derive(Debug, Deserialize)]
struct Config {
    game: GameConfig,
    #[serde(default)]
    webrtc: WebRtcConfig,
    #[serde(default)]
    remote: Vec<RemotePeer>,
    #[serde(default)]
    #[serde(rename = "steampipe_command")]
    _steampipe_command: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GameConfig {
    binary: PathBuf,
    #[serde(default)]
    build_command: Option<String>,
    #[serde(default)]
    project_root: Option<PathBuf>,
    #[serde(default)]
    assets_dir: Option<PathBuf>,
    #[serde(default = "default_listener_log", alias = "local_log")]
    listener_log: PathBuf,
    #[serde(default = "default_joiner_log")]
    joiner_log: PathBuf,
    #[serde(default)]
    env: Vec<EnvPair>,
    #[serde(default)]
    host_args: Vec<String>,
    #[serde(default)]
    join_args: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct EnvPair {
    name: String,
    value: String,
}

#[derive(Debug, Deserialize)]
struct WebRtcConfig {
    #[serde(default = "default_local_ip")]
    local_ip: IpAddr,
    #[serde(default = "default_webrtc_port")]
    port: u16,
    #[serde(default = "default_timeout_secs")]
    timeout_secs: u64,
    #[serde(default = "default_max_runs")]
    max_runs: u32,
}

impl Default for WebRtcConfig {
    fn default() -> Self {
        Self {
            local_ip: default_local_ip(),
            port: default_webrtc_port(),
            timeout_secs: default_timeout_secs(),
            max_runs: default_max_runs(),
        }
    }
}

#[derive(Debug, Deserialize)]
struct RemotePeer {
    name: String,
    host: String,
    remote_dir: String,
    #[serde(default = "default_remote_log")]
    log_file: String,
    #[serde(default)]
    binary_name: Option<String>,
    #[serde(default)]
    assets_dir: Option<PathBuf>,
    #[serde(default)]
    deploy: Vec<DeployPath>,
    #[serde(default)]
    env: Vec<EnvPair>,
    #[serde(default)]
    join_args: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct DeployPath {
    source: PathBuf,
    target: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
enum RunStatus {
    Pass,
    Fail,
    Timeout,
}

#[derive(Debug, Clone, Serialize)]
struct RunReport {
    run: u32,
    status: RunStatus,
    label: String,
    duration_secs: u64,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Webrtc1v1(args) => run_webrtc_1v1(args),
    }
}

fn run_webrtc_1v1(args: WebRtc1v1Args) -> Result<()> {
    let config = load_config(&args.config)?;
    let remote = match args.remote.as_deref() {
        Some(name) => Some(select_remote(&config, name)?),
        None => None,
    };
    let max_runs = args.max_runs.unwrap_or(config.webrtc.max_runs);
    let timeout = Duration::from_secs(args.timeout.unwrap_or(config.webrtc.timeout_secs));
    let port = args.webrtc_port.unwrap_or(config.webrtc.port);
    let local_ip = args.local_ip.unwrap_or(config.webrtc.local_ip);

    if !args.no_build {
        run_build(&config, args.dry_run)?;
    }

    if let Some(remote) = remote
        && !args.no_deploy
    {
        deploy(&config, remote, args.dry_run)?;
    }

    let mut passed = 0;
    let mut failed = 0;
    for run in 1..=max_runs {
        println!("=== RUN {run}/{max_runs} ===");
        let report = run_one(&config, remote, run, port, local_ip, timeout, args.dry_run)?;
        emit_report(args.output_format, &report)?;
        match report.status {
            RunStatus::Pass => passed += 1,
            RunStatus::Fail | RunStatus::Timeout => {
                failed += 1;
                if args.stop_on_failure {
                    break;
                }
            }
        }
    }

    println!("=== SUMMARY ===");
    println!("{passed}/{max_runs} passed, {failed} failed");
    if failed == 0 {
        Ok(())
    } else {
        bail!("{failed} WebRTC 1v1 run(s) failed")
    }
}

fn load_config(path: &Path) -> Result<Config> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("failed to read config {}", path.display()))?;
    toml::from_str(&text).with_context(|| format!("failed to parse config {}", path.display()))
}

fn select_remote<'a>(config: &'a Config, name: &str) -> Result<&'a RemotePeer> {
    config
        .remote
        .iter()
        .find(|remote| remote.name == name)
        .ok_or_else(|| anyhow!("remote peer '{name}' is not defined"))
}

fn run_build(config: &Config, dry_run: bool) -> Result<()> {
    let Some(command) = config.game.build_command.as_deref() else {
        return Ok(());
    };
    println!("==> Building: {command}");
    if dry_run {
        return Ok(());
    }
    let status = shell_command(command)
        .current_dir(project_root(config)?)
        .status()
        .context("failed to run build command")?;
    ensure_success(status, "build command")
}

fn deploy(config: &Config, remote: &RemotePeer, dry_run: bool) -> Result<()> {
    let project_root = project_root(config)?;
    run_ssh(
        &remote.host,
        &format!("mkdir -p {}", shell_quote(&remote.remote_dir)),
        dry_run,
    )?;

    let binary_name = remote_binary_name(config, remote)?;
    let remote_binary = format!("{}/{}", remote.remote_dir, binary_name);
    run_rsync(
        &config.game.binary,
        &remote.host,
        &remote_binary,
        project_root,
        dry_run,
    )?;

    if let Some(assets_dir) = config.game.assets_dir.as_ref() {
        let target = remote
            .assets_dir
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_else(|| format!("{}/assets", remote.remote_dir));
        run_rsync(assets_dir, &remote.host, &target, project_root, dry_run)?;
    }

    for item in &remote.deploy {
        run_rsync(
            &item.source,
            &remote.host,
            &item.target,
            project_root,
            dry_run,
        )?;
    }

    run_ssh(
        &remote.host,
        &format!("chmod +x {}", shell_quote(&remote_binary)),
        dry_run,
    )
}

fn run_one(
    config: &Config,
    remote: Option<&RemotePeer>,
    run: u32,
    port: u16,
    local_ip: IpAddr,
    timeout: Duration,
    dry_run: bool,
) -> Result<RunReport> {
    let started = Instant::now();
    let result = match remote {
        Some(remote) => run_one_remote(config, remote, port, local_ip, timeout, dry_run),
        None => run_one_local(config, port, local_ip, timeout, dry_run),
    };

    let duration_secs = started.elapsed().as_secs();
    match result {
        Ok(label) => Ok(RunReport {
            run,
            status: RunStatus::Pass,
            label,
            duration_secs,
        }),
        Err(error) if error.to_string().contains("timed out") => Ok(RunReport {
            run,
            status: RunStatus::Timeout,
            label: error.to_string(),
            duration_secs,
        }),
        Err(error) => Ok(RunReport {
            run,
            status: RunStatus::Fail,
            label: error.to_string(),
            duration_secs,
        }),
    }
}

fn run_one_local(
    config: &Config,
    port: u16,
    local_ip: IpAddr,
    timeout: Duration,
    dry_run: bool,
) -> Result<String> {
    let listener_log = config.game.listener_log.clone();
    let joiner_log = config.game.joiner_log.clone();
    remove_if_exists(&listener_log)?;
    remove_if_exists(&joiner_log)?;

    let mut listener = launch_listener(config, port, &listener_log, dry_run)?;
    if dry_run {
        let join_addr =
            format!("/ip4/{local_ip}/udp/{port}/webrtc-direct/certhash/uEiDryRunCerthash");
        let mut joiner = launch_local_joiner(config, &join_addr, &joiner_log, dry_run)?;
        kill_child(&mut listener);
        kill_child(&mut joiner);
        return Ok("DRY_RUN".into());
    }

    let result = run_one_local_inner(
        config,
        &mut listener,
        &listener_log,
        &joiner_log,
        local_ip,
        timeout,
    );
    kill_child(&mut listener);
    result
}

fn run_one_remote(
    config: &Config,
    remote: &RemotePeer,
    port: u16,
    local_ip: IpAddr,
    timeout: Duration,
    dry_run: bool,
) -> Result<String> {
    let listener_log = config.game.listener_log.clone();
    remove_if_exists(&listener_log)?;

    stop_remote(remote, dry_run)?;
    let mut listener = launch_listener(config, port, &listener_log, dry_run)?;
    if dry_run {
        let join_addr =
            format!("/ip4/{local_ip}/udp/{port}/webrtc-direct/certhash/uEiDryRunCerthash");
        launch_remote(config, remote, &join_addr, dry_run)?;
        kill_child(&mut listener);
        return Ok("DRY_RUN".into());
    }

    let result = run_one_remote_inner(
        config,
        remote,
        &mut listener,
        &listener_log,
        local_ip,
        timeout,
    );
    stop_remote(remote, false)?;
    kill_child(&mut listener);
    result
}

fn run_one_local_inner(
    config: &Config,
    listener: &mut Child,
    listener_log: &Path,
    joiner_log: &Path,
    local_ip: IpAddr,
    timeout: Duration,
) -> Result<String> {
    let raw_addr = wait_for_join_addr(listener_log, timeout)?;
    let join_addr = rewrite_webrtc_join_addr(&raw_addr, local_ip)?;
    println!("WEBRTC_JOIN_ADDR={join_addr}");
    let mut joiner = launch_local_joiner(config, &join_addr, joiner_log, false)?;

    let started = Instant::now();
    let mut listener_exited = None;
    let mut joiner_exited = None;
    loop {
        let listener_log_text = read_lossy(listener_log);
        let joiner_log_text = read_lossy(joiner_log);

        if listener_log_text.contains("GAME OVER") && joiner_log_text.contains("GAME OVER") {
            kill_child(&mut joiner);
            return Ok("GAME OVER".into());
        }

        if let Some(label) = classify_non_success_log(&listener_log_text) {
            kill_child(&mut joiner);
            bail!("local listening peer reported {label}");
        }
        if let Some(label) = classify_non_success_log(&joiner_log_text) {
            kill_child(&mut joiner);
            bail!("local joining peer reported {label}");
        }

        if listener_exited.is_none() {
            listener_exited = listener
                .try_wait()
                .context("failed to poll local listening peer")?;
        }
        if joiner_exited.is_none() {
            joiner_exited = joiner
                .try_wait()
                .context("failed to poll local joining peer")?;
        }
        if let Some(status) = listener_exited {
            kill_child(&mut joiner);
            bail!("local listening peer exited before GAME OVER: {status}");
        }
        if let Some(status) = joiner_exited {
            bail!("local joining peer exited before GAME OVER: {status}");
        }

        if started.elapsed() > timeout {
            kill_child(&mut joiner);
            bail!(
                "timed out after {}s waiting for both local peers to reach GAME OVER",
                timeout.as_secs()
            );
        }

        thread::sleep(Duration::from_secs(1));
    }
}

fn run_one_remote_inner(
    config: &Config,
    remote: &RemotePeer,
    listener: &mut Child,
    listener_log: &Path,
    local_ip: IpAddr,
    timeout: Duration,
) -> Result<String> {
    let raw_addr = wait_for_join_addr(listener_log, timeout)?;
    let join_addr = rewrite_webrtc_join_addr(&raw_addr, local_ip)?;
    println!("WEBRTC_JOIN_ADDR={join_addr}");
    launch_remote(config, remote, &join_addr, false)?;

    let started = Instant::now();
    loop {
        if let Some(status) = listener
            .try_wait()
            .context("failed to poll local listening peer")?
        {
            bail!("local listening peer exited early in remote mode: {status}");
        }

        let listener_log_text = read_lossy(listener_log);
        if let Some(label) = classify_log(&listener_log_text) {
            match label {
                "GAME OVER" => return Ok(label.into()),
                other => bail!("local listening peer reported {other}"),
            }
        }

        let remote_log = remote_log(remote)?;
        if let Some(label) = classify_log(&remote_log) {
            match label {
                "GAME OVER" => {
                    if listener_log_text.contains("GAME OVER") {
                        return Ok(label.into());
                    }
                }
                other => bail!("remote joining peer reported {other}"),
            }
        }

        if started.elapsed() > timeout {
            bail!(
                "timed out after {}s waiting for GAME OVER",
                timeout.as_secs()
            );
        }

        thread::sleep(Duration::from_secs(1));
    }
}

fn launch_listener(config: &Config, port: u16, log_path: &Path, dry_run: bool) -> Result<Child> {
    let args = listener_args(config, port);
    let command = command_line(&config.game.binary, &args);
    println!("==> Local listening peer: {command}");
    if dry_run {
        return spawn_noop_child();
    }

    let log = File::create(log_path)
        .with_context(|| format!("failed to create local log {}", log_path.display()))?;
    let log_err = log.try_clone().context("failed to clone local log file")?;
    let mut cmd = Command::new(&config.game.binary);
    cmd.args(args)
        .current_dir(project_root(config)?)
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err));
    apply_env(&mut cmd, &config.game.env);
    cmd.env("BEVY_ASSET_ROOT", project_root(config)?);
    spawn_local_process(cmd, "failed to launch local listening peer")
}

fn launch_local_joiner(
    config: &Config,
    join_addr: &str,
    log_path: &Path,
    dry_run: bool,
) -> Result<Child> {
    let args = joiner_args(config, join_addr);
    let command = command_line(&config.game.binary, &args);
    println!("==> Local joining peer: {command}");
    if dry_run {
        return spawn_noop_child();
    }

    let log = File::create(log_path)
        .with_context(|| format!("failed to create joiner log {}", log_path.display()))?;
    let log_err = log.try_clone().context("failed to clone joiner log file")?;
    let mut cmd = Command::new(&config.game.binary);
    cmd.args(args)
        .current_dir(project_root(config)?)
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err));
    apply_env(&mut cmd, &config.game.env);
    cmd.env("BEVY_ASSET_ROOT", project_root(config)?);
    spawn_local_process(cmd, "failed to launch local joining peer")
}

fn spawn_local_process(mut cmd: Command, context: &'static str) -> Result<Child> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        cmd.process_group(0);
    }
    cmd.spawn().context(context)
}

fn listener_args(config: &Config, port: u16) -> Vec<String> {
    let mut args = vec![
        "--auto-host-webrtc".to_string(),
        "--webrtc-port".to_string(),
        port.to_string(),
        "--auto-play".to_string(),
    ];
    args.extend(config.game.host_args.clone());
    args
}

fn joiner_args(config: &Config, join_addr: &str) -> Vec<String> {
    let mut args = vec![
        "--auto-join-webrtc".to_string(),
        "--webrtc-addr".to_string(),
        join_addr.to_string(),
        "--auto-play".to_string(),
        "--headless".to_string(),
    ];
    args.extend(config.game.join_args.clone());
    args
}

fn launch_remote(
    config: &Config,
    remote: &RemotePeer,
    join_addr: &str,
    dry_run: bool,
) -> Result<()> {
    let binary_name = remote_binary_name(config, remote)?;
    let mut args = joiner_args(config, join_addr);
    args.extend(remote.join_args.clone());

    let mut env = String::new();
    for pair in &config.game.env {
        env.push_str(&format!("{}={} ", pair.name, shell_quote(&pair.value)));
    }
    for pair in &remote.env {
        env.push_str(&format!("{}={} ", pair.name, shell_quote(&pair.value)));
    }

    let remote_binary = format!("{}/{}", remote.remote_dir, binary_name);
    let command = format!(
        "cd {} && {}nohup {} {} > {} 2>&1 < /dev/null &",
        shell_quote(&remote.remote_dir),
        env,
        shell_quote(&remote_binary),
        shell_args(&args),
        shell_quote(&remote.log_file),
    );
    println!("==> Remote joining peer on {}: {}", remote.name, command);
    run_ssh(&remote.host, &command, dry_run)
}

fn stop_remote(remote: &RemotePeer, dry_run: bool) -> Result<()> {
    run_ssh(
        &remote.host,
        "pkill -x chessbender 2>/dev/null || true",
        dry_run,
    )
}

fn wait_for_join_addr(log_path: &Path, timeout: Duration) -> Result<String> {
    let started = Instant::now();
    loop {
        let text = read_lossy(log_path);
        if let Some(addr) = parse_webrtc_join_addr(&text) {
            return Ok(addr);
        }
        if let Some(label) = classify_log(&text)
            && label != "GAME OVER"
        {
            bail!("local listening peer reported {label} before WebRTC address was emitted");
        }
        if started.elapsed() > timeout {
            bail!(
                "timed out after {}s waiting for WEBRTC_JOIN_ADDR",
                timeout.as_secs()
            );
        }
        thread::sleep(Duration::from_millis(250));
    }
}

fn parse_webrtc_join_addr(log: &str) -> Option<String> {
    let re = Regex::new(r"WEBRTC_JOIN_ADDR=(\S+)").ok()?;
    re.captures_iter(log)
        .last()
        .and_then(|captures| captures.get(1))
        .map(|match_| match_.as_str().to_string())
}

fn rewrite_webrtc_join_addr(addr: &str, local_ip: IpAddr) -> Result<String> {
    let parts: Vec<&str> = addr.split('/').collect();
    if parts.len() < 4 {
        bail!("invalid multiaddr: {addr}");
    }

    let replacement_protocol = match local_ip {
        IpAddr::V4(_) => "ip4",
        IpAddr::V6(_) => "ip6",
    };
    let replacement_ip = local_ip.to_string();
    let mut rewritten: Vec<String> = parts.iter().map(|part| (*part).to_string()).collect();

    let mut index = 1;
    while index + 1 < rewritten.len() {
        let protocol = rewritten[index].as_str();
        if protocol == "ip4" || protocol == "ip6" {
            let value = rewritten[index + 1].as_str();
            if is_non_routable_listen_ip(value) {
                rewritten[index] = replacement_protocol.to_string();
                rewritten[index + 1] = replacement_ip;
            }
            break;
        }
        index += 2;
    }

    Ok(rewritten.join("/"))
}

fn is_non_routable_listen_ip(value: &str) -> bool {
    matches!(value, "0.0.0.0" | "127.0.0.1" | "::" | "::1" | "localhost")
}

fn classify_log(log: &str) -> Option<&'static str> {
    if log.contains("GAME OVER") {
        return Some("GAME OVER");
    }
    const FATAL_MARKERS: &[(&str, &str)] = &[
        ("[FATAL]", "FATAL"),
        ("panic", "PANIC"),
        ("DAG_VIOLATION", "DAG_VIOLATION"),
        ("DESYNC", "DESYNC"),
        ("CONNECTION_LOST", "CONNECTION_LOST"),
        ("PEER_READY_TIMEOUT", "PEER_READY_TIMEOUT"),
        ("Graceful shutdown: exit_code=", "GRACEFUL_SHUTDOWN_ERROR"),
    ];
    for (needle, label) in FATAL_MARKERS {
        if log.contains(needle) && !log.contains("Graceful shutdown: exit_code=0") {
            return Some(label);
        }
    }
    None
}

fn classify_non_success_log(log: &str) -> Option<&'static str> {
    match classify_log(log) {
        Some("GAME OVER") | None => None,
        Some(label) => Some(label),
    }
}

fn emit_report(format: OutputFormat, report: &RunReport) -> Result<()> {
    match format {
        OutputFormat::Text => {
            println!(
                "--- {:?} ({}, {}s) ---",
                report.status, report.label, report.duration_secs
            );
            Ok(())
        }
        OutputFormat::Jsonl => {
            println!("{}", serde_json::to_string(report)?);
            Ok(())
        }
    }
}

fn remote_log(remote: &RemotePeer) -> Result<String> {
    let command = format!(
        "tail -n 200 {}/{} 2>/dev/null || true",
        shell_quote(&remote.remote_dir),
        shell_quote(&remote.log_file),
    );
    let output = Command::new("ssh")
        .arg(&remote.host)
        .arg(command)
        .output()
        .with_context(|| format!("failed to read remote log from {}", remote.host))?;
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn run_ssh(host: &str, command: &str, dry_run: bool) -> Result<()> {
    println!("ssh {host} {}", shell_quote(command));
    if dry_run {
        return Ok(());
    }
    let status = Command::new("ssh")
        .arg(host)
        .arg(command)
        .status()
        .with_context(|| format!("failed to run ssh command on {host}"))?;
    ensure_success(status, "ssh command")
}

fn run_rsync(
    source: &Path,
    host: &str,
    target: &str,
    project_root: &Path,
    dry_run: bool,
) -> Result<()> {
    let resolved = resolve_path(project_root, source);
    println!("rsync -az {} {host}:{target}", resolved.display());
    if dry_run {
        return Ok(());
    }
    let status = Command::new("rsync")
        .arg("-az")
        .arg(&resolved)
        .arg(format!("{host}:{target}"))
        .status()
        .context("failed to run rsync")?;
    ensure_success(status, "rsync")
}

fn shell_command(command: &str) -> Command {
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(command);
    cmd
}

fn command_line(binary: &Path, args: &[String]) -> String {
    format!("{} {}", binary.display(), shell_args(args))
}

fn shell_args(args: &[String]) -> String {
    args.iter().map(shell_quote).collect::<Vec<_>>().join(" ")
}

fn shell_quote(value: impl AsRef<str>) -> String {
    let value = value.as_ref();
    if value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || "-_./:=@".contains(ch))
    {
        return value.to_string();
    }
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn apply_env(cmd: &mut Command, env: &[EnvPair]) {
    for pair in env {
        cmd.env(&pair.name, &pair.value);
    }
}

fn project_root(config: &Config) -> Result<&Path> {
    config
        .game
        .project_root
        .as_deref()
        .or_else(|| Some(Path::new(".")))
        .ok_or_else(|| anyhow!("project root is not configured"))
}

fn resolve_path(root: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    }
}

fn remote_binary_name(config: &Config, remote: &RemotePeer) -> Result<String> {
    if let Some(name) = &remote.binary_name {
        return Ok(name.clone());
    }
    config
        .game
        .binary
        .file_name()
        .and_then(OsStr::to_str)
        .map(str::to_string)
        .ok_or_else(|| anyhow!("game.binary must have a file name"))
}

fn ensure_success(status: ExitStatus, label: &str) -> Result<()> {
    if status.success() {
        Ok(())
    } else {
        bail!("{label} failed with status {status}")
    }
}

fn read_lossy(path: &Path) -> String {
    let Ok(mut file) = File::open(path) else {
        return String::new();
    };
    let mut text = String::new();
    let _ = file.read_to_string(&mut text);
    text
}

fn remove_if_exists(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("failed to remove {}", path.display())),
    }
}

fn kill_child(child: &mut Child) {
    #[cfg(unix)]
    {
        let _ = Command::new("kill")
            .arg("-TERM")
            .arg(format!("-{}", child.id()))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn spawn_noop_child() -> Result<Child> {
    Command::new("sh")
        .arg("-c")
        .arg("sleep 0")
        .spawn()
        .context("failed to spawn dry-run placeholder process")
}

fn default_listener_log() -> PathBuf {
    PathBuf::from("fragpipe-listener.log")
}

fn default_joiner_log() -> PathBuf {
    PathBuf::from("fragpipe-joiner.log")
}

fn default_remote_log() -> String {
    "game.log".into()
}

fn default_local_ip() -> IpAddr {
    "127.0.0.1".parse().expect("valid default IP")
}

fn default_webrtc_port() -> u16 {
    27200
}

fn default_timeout_secs() -> u64 {
    300
}

fn default_max_runs() -> u32 {
    1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_last_webrtc_join_addr() {
        let log = "noise\nWEBRTC_JOIN_ADDR=/ip4/0.0.0.0/udp/27200/webrtc-direct/certhash/abc\n";
        assert_eq!(
            parse_webrtc_join_addr(log).as_deref(),
            Some("/ip4/0.0.0.0/udp/27200/webrtc-direct/certhash/abc")
        );
    }

    #[test]
    fn rewrites_wildcard_addr_and_preserves_certhash() {
        let addr = "/ip4/0.0.0.0/udp/27200/webrtc-direct/certhash/uEiHash";
        let rewritten = rewrite_webrtc_join_addr(addr, "10.0.0.5".parse().unwrap()).unwrap();
        assert_eq!(
            rewritten,
            "/ip4/10.0.0.5/udp/27200/webrtc-direct/certhash/uEiHash"
        );
    }

    #[test]
    fn rewrites_loopback_ipv4_to_ipv6_when_configured() {
        let addr = "/ip4/127.0.0.1/udp/27200/webrtc-direct/certhash/uEiHash";
        let rewritten = rewrite_webrtc_join_addr(addr, "2001:db8::1".parse().unwrap()).unwrap();
        assert_eq!(
            rewritten,
            "/ip6/2001:db8::1/udp/27200/webrtc-direct/certhash/uEiHash"
        );
    }

    #[test]
    fn leaves_routable_addr_unchanged() {
        let addr = "/ip4/10.0.0.12/udp/27200/webrtc-direct/certhash/uEiHash";
        let rewritten = rewrite_webrtc_join_addr(addr, "10.0.0.5".parse().unwrap()).unwrap();
        assert_eq!(rewritten, addr);
    }

    #[test]
    fn builds_expected_join_args() {
        let config = test_config();
        let args = joiner_args(
            &config,
            "/ip4/10.0.0.5/udp/27200/webrtc-direct/certhash/uEiHash",
        );
        assert_eq!(
            shell_args(&args),
            "--auto-join-webrtc --webrtc-addr /ip4/10.0.0.5/udp/27200/webrtc-direct/certhash/uEiHash --auto-play --headless"
        );
    }

    #[test]
    fn builds_expected_listener_args() {
        let config = test_config();
        assert_eq!(
            listener_args(&config, 27200),
            vec![
                "--auto-host-webrtc",
                "--webrtc-port",
                "27200",
                "--auto-play"
            ]
        );
    }

    #[test]
    fn config_without_remote_is_valid() {
        let config: Config = toml::from_str(
            r#"
            [game]
            binary = "target/release/chessbender"

            [webrtc]
            local_ip = "127.0.0.1"
            "#,
        )
        .unwrap();

        assert!(config.remote.is_empty());
        assert_eq!(
            config.game.listener_log,
            PathBuf::from("fragpipe-listener.log")
        );
        assert_eq!(config.game.joiner_log, PathBuf::from("fragpipe-joiner.log"));
    }

    #[test]
    fn classifies_pass_and_fatal_logs() {
        assert_eq!(classify_log("turn 3\nGAME OVER\n"), Some("GAME OVER"));
        assert_eq!(classify_log("[FATAL] desync"), Some("FATAL"));
        assert_eq!(classify_log("all good"), None);
    }

    fn test_config() -> Config {
        Config {
            game: GameConfig {
                binary: PathBuf::from("target/release/chessbender"),
                build_command: None,
                project_root: None,
                assets_dir: None,
                listener_log: default_listener_log(),
                joiner_log: default_joiner_log(),
                env: Vec::new(),
                host_args: Vec::new(),
                join_args: Vec::new(),
            },
            webrtc: WebRtcConfig::default(),
            remote: Vec::new(),
            _steampipe_command: None,
        }
    }
}
