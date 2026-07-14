use std::fs;
use std::fs::File;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::android;
use crate::config::{
    AndroidConfig, AndroidTarget, Config, RemotePeer, load_config, project_root, resolve_path,
    select_remote,
};
use crate::logwatch::{LogSignal, classify_log, classify_non_success_log, read_lossy};
use crate::process::{
    check_interrupted, kill_child, remove_if_exists, run_build, run_shell_command, spawn_logged,
    spawn_process_group,
};
use crate::ssh;
use crate::util::command_line;
use crate::webrtc::{joiner_args, listener_args, parse_join_addr_prefer_ip, rewrite_join_addr};

pub struct WebRtcRunOptions {
    pub config_path: PathBuf,
    pub max_runs: Option<u32>,
    pub timeout_secs: Option<u64>,
    pub stop_on_failure: bool,
    pub no_build: bool,
    pub no_deploy: bool,
    pub remote: Option<String>,
    pub local_ip: Option<IpAddr>,
    pub webrtc_port: Option<u16>,
    pub dry_run: bool,
    pub output_format: OutputFormat,
}

pub struct InternetRunOptions {
    pub config_path: PathBuf,
    pub max_runs: Option<u32>,
    pub timeout_secs: Option<u64>,
    pub stop_on_failure: bool,
    pub workdir: Option<PathBuf>,
    pub game_bin: Option<PathBuf>,
    pub rdv_bin: Option<PathBuf>,
    pub asset_root: Option<PathBuf>,
    pub log_dir: Option<PathBuf>,
    pub pass_marker: Option<String>,
    pub dry_run: bool,
    pub output_format: OutputFormat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    Text,
    Jsonl,
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

pub fn run_internet_1v1(options: InternetRunOptions) -> Result<()> {
    let config = load_config(&options.config_path)?;
    println!("Fragpipe project: {}", config.game.name);

    let config_dir = options
        .config_path
        .parent()
        .unwrap_or_else(|| Path::new("."));
    let workdir = match options.workdir {
        Some(path) => path,
        None => config
            .game
            .project_root
            .as_ref()
            .map(|path| resolve_path(config_dir, path))
            .context("--workdir is required unless [game].project_root is set")?,
    };
    let script = workdir.join("dev/netns/internet-1v1-forced-relay.sh");
    if !script.exists() {
        bail!(
            "internet smoke script is missing: {} (expected under --workdir)",
            script.display()
        );
    }

    let game_bin = options
        .game_bin
        .or_else(|| config.internet.game_bin.clone())
        .unwrap_or_else(|| config.game.binary.clone());
    let rdv_bin = options
        .rdv_bin
        .or_else(|| config.internet.rdv_bin.clone())
        .context("--rdv-bin is required unless [internet].rdv_bin is set")?;
    let asset_root = options
        .asset_root
        .or_else(|| config.internet.asset_root.clone())
        .unwrap_or_else(|| PathBuf::from("assets"));
    let base_log_dir = options
        .log_dir
        .or_else(|| config.internet.log_dir.clone())
        .unwrap_or_else(|| PathBuf::from("logs/fragpipe/internet-1v1"));
    let max_runs = options.max_runs.unwrap_or(config.internet.max_runs);
    validate_max_runs(max_runs)?;
    let timeout_secs = options.timeout_secs.unwrap_or(config.internet.timeout_secs);
    let timeout = Duration::from_secs(timeout_secs);
    let pass_marker = options
        .pass_marker
        .unwrap_or_else(|| config.internet.pass_marker.clone());

    let game_bin = resolve_path(&workdir, &game_bin);
    let rdv_bin = resolve_path(&workdir, &rdv_bin);
    let asset_root = resolve_path(&workdir, &asset_root);
    let base_log_dir = resolve_path(&workdir, &base_log_dir);

    let mut passed = 0;
    let mut failed = 0;
    for run in 1..=max_runs {
        println!("=== RUN {run}/{max_runs} ===");
        let run_log_dir = base_log_dir
            .join(timestamp_secs().to_string())
            .join(format!("run-{run}"));
        let report = run_one_internet(InternetOneRunOptions {
            run,
            script: &script,
            workdir: &workdir,
            game_bin: &game_bin,
            rdv_bin: &rdv_bin,
            asset_root: &asset_root,
            log_dir: &run_log_dir,
            timeout,
            timeout_secs,
            pass_marker: &pass_marker,
            dry_run: options.dry_run,
        });
        emit_report(options.output_format, &report)?;
        match report.status {
            RunStatus::Pass => passed += 1,
            RunStatus::Fail | RunStatus::Timeout => {
                failed += 1;
                if options.stop_on_failure {
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
        bail!("{failed} internet 1v1 run(s) failed")
    }
}

struct InternetOneRunOptions<'a> {
    run: u32,
    script: &'a Path,
    workdir: &'a Path,
    game_bin: &'a Path,
    rdv_bin: &'a Path,
    asset_root: &'a Path,
    log_dir: &'a Path,
    timeout: Duration,
    timeout_secs: u64,
    pass_marker: &'a str,
    dry_run: bool,
}

fn run_one_internet(options: InternetOneRunOptions<'_>) -> RunReport {
    let started = Instant::now();
    let result = run_one_internet_inner(&options);
    let duration_secs = started.elapsed().as_secs();
    match result {
        Ok(label) => RunReport {
            run: options.run,
            status: RunStatus::Pass,
            label,
            duration_secs,
        },
        Err(error) if error.to_string().contains("timed out") => RunReport {
            run: options.run,
            status: RunStatus::Timeout,
            label: error.to_string(),
            duration_secs,
        },
        Err(error) => RunReport {
            run: options.run,
            status: RunStatus::Fail,
            label: error.to_string(),
            duration_secs,
        },
    }
}

fn run_one_internet_inner(options: &InternetOneRunOptions<'_>) -> Result<String> {
    let wrapper_log = options.log_dir.join("fragpipe-internet-1v1.log");
    let command = format!(
        "GAME_BIN={} RDV_BIN={} ASSET_ROOT={} LOG_DIR={} TIMEOUT_SECS={} PASS_MARKER={} {}",
        options.game_bin.display(),
        options.rdv_bin.display(),
        options.asset_root.display(),
        options.log_dir.display(),
        options.timeout_secs,
        options.pass_marker,
        options.script.display(),
    );
    println!("==> Forced-relay internet 1v1: {command}");
    println!("logs: {}", options.log_dir.display());
    if options.dry_run {
        return Ok("DRY_RUN".into());
    }

    fs::create_dir_all(options.log_dir)
        .with_context(|| format!("failed to create log dir {}", options.log_dir.display()))?;
    let log = File::create(&wrapper_log)
        .with_context(|| format!("failed to create log {}", wrapper_log.display()))?;
    let log_err = log
        .try_clone()
        .context("failed to clone wrapper log file")?;
    let mut command = Command::new(options.script);
    command
        .current_dir(options.workdir)
        .env("GAME_BIN", options.game_bin)
        .env("RDV_BIN", options.rdv_bin)
        .env("ASSET_ROOT", options.asset_root)
        .env("LOG_DIR", options.log_dir)
        .env("TIMEOUT_SECS", options.timeout_secs.to_string())
        .env("PASS_MARKER", options.pass_marker)
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err));
    let mut child = spawn_process_group(command, "failed to launch internet smoke")
        .with_context(|| format!("failed to launch {}", options.script.display()))?;

    let started = Instant::now();
    let status = loop {
        if let Err(error) = check_interrupted() {
            kill_child(&mut child);
            return Err(error);
        }
        if let Some(status) = child.try_wait().context("failed to poll internet smoke")? {
            break status;
        }
        if started.elapsed() > options.timeout + Duration::from_secs(30) {
            kill_child(&mut child);
            let output = read_lossy(&wrapper_log);
            bail!(
                "timed out after {}s waiting for internet smoke to exit{}",
                options.timeout.as_secs() + 30,
                internet_log_excerpt(&output)
            );
        }
        thread::sleep(Duration::from_millis(250));
    };

    let output = read_lossy(&wrapper_log);
    print!("{output}");
    if status.success() && output.contains("PASS:") && output.contains(options.pass_marker) {
        return Ok(first_marker_line(&output, "PASS:")
            .unwrap_or(options.pass_marker)
            .into());
    }
    if let Some(line) = first_marker_line(&output, "FAIL:") {
        bail!("{line}");
    }
    if let Some(line) = first_marker_line(&output, "FATAL:") {
        bail!("{line}");
    }
    if status.success() {
        bail!(
            "internet smoke exited successfully without PASS marker `{}`",
            options.pass_marker
        );
    }
    bail!("internet smoke failed with status {status}")
}

fn first_marker_line<'a>(output: &'a str, marker: &str) -> Option<&'a str> {
    output.lines().find(|line| line.contains(marker))
}

fn internet_log_excerpt(output: &str) -> String {
    let lines = output.lines().rev().take(8).collect::<Vec<_>>();
    if lines.is_empty() {
        return String::new();
    }
    let mut excerpt = String::from("; log tail:");
    for line in lines.into_iter().rev() {
        excerpt.push('\n');
        excerpt.push_str(line);
    }
    excerpt
}

pub fn run_webrtc_1v1(options: WebRtcRunOptions) -> Result<()> {
    let config = load_config(&options.config_path)?;
    println!("Fragpipe project: {}", config.game.name);
    let remote = match options.remote.as_deref() {
        Some(name) => Some(select_remote(&config, name)?),
        None => None,
    };
    let max_runs = options.max_runs.unwrap_or(config.webrtc.max_runs);
    validate_max_runs(max_runs)?;
    let timeout = Duration::from_secs(options.timeout_secs.unwrap_or(config.webrtc.timeout_secs));
    let port = options.webrtc_port.unwrap_or(config.webrtc.port);
    let local_ip = options.local_ip.unwrap_or(config.webrtc.local_ip);

    if !options.no_build {
        run_build(&config, options.dry_run)?;
    }

    if let Some(remote) = remote
        && !options.no_deploy
    {
        ssh::deploy(&config, remote, options.dry_run)?;
    }

    let mut passed = 0;
    let mut failed = 0;
    for run in 1..=max_runs {
        println!("=== RUN {run}/{max_runs} ===");
        let report = run_one(
            &config,
            remote,
            run,
            port,
            local_ip,
            timeout,
            options.dry_run,
        )?;
        emit_report(options.output_format, &report)?;
        match report.status {
            RunStatus::Pass => passed += 1,
            RunStatus::Fail | RunStatus::Timeout => {
                failed += 1;
                if options.stop_on_failure {
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
    if dry_run {
        let listener_args = listener_args(config, port);
        println!(
            "==> Local listening peer: {}",
            command_line(&config.game.binary, &listener_args)
        );
        let join_addr =
            format!("/ip4/{local_ip}/udp/{port}/webrtc-direct/certhash/uEiDryRunCerthash");
        let joining_args = joiner_args(config, &join_addr);
        println!(
            "==> Local joining peer: {}",
            command_line(&config.game.binary, &joining_args)
        );
        return Ok("DRY_RUN".into());
    }
    remove_if_exists(&listener_log)?;
    remove_if_exists(&joiner_log)?;

    let mut listener = launch_listener(config, port, &listener_log)?;

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
    ssh::stop_remote(config, remote, dry_run)?;
    if dry_run {
        let listener_args = listener_args(config, port);
        println!(
            "==> Local listening peer: {}",
            command_line(&config.game.binary, &listener_args)
        );
        let join_addr =
            format!("/ip4/{local_ip}/udp/{port}/webrtc-direct/certhash/uEiDryRunCerthash");
        let mut args = joiner_args(config, &join_addr);
        args.extend(remote.join_args.clone());
        ssh::launch_remote(config, remote, &args, dry_run)?;
        return Ok("DRY_RUN".into());
    }

    remove_if_exists(&listener_log)?;
    let mut listener = launch_listener(config, port, &listener_log)?;

    let result = run_one_remote_inner(
        config,
        remote,
        &mut listener,
        &listener_log,
        local_ip,
        timeout,
    );
    let stop_result = ssh::stop_remote(config, remote, false);
    kill_child(&mut listener);
    stop_result?;
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
    let raw_addr = wait_for_join_addr(config, listener, listener_log, local_ip, timeout)?;
    let join_addr = rewrite_join_addr(&raw_addr, local_ip)?;
    println!("{}{}", config.webrtc.join_addr_marker, join_addr);
    let joiner_args = joiner_args(config, &join_addr);
    let mut joiner = spawn_logged(config, &joiner_args, joiner_log, "Local joining peer")?;

    let started = Instant::now();
    let mut listener_exited = None;
    let mut joiner_exited = None;
    loop {
        if let Err(error) = check_interrupted() {
            kill_child(&mut joiner);
            return Err(error);
        }
        let listener_log_text = read_lossy(listener_log);
        let joiner_log_text = read_lossy(joiner_log);

        if let Some(LogSignal::Pass(label)) = classify_log(&config.process, &listener_log_text)
            && matches!(
                classify_log(&config.process, &joiner_log_text),
                Some(LogSignal::Pass(_))
            )
        {
            kill_child(&mut joiner);
            return Ok(label.into());
        }

        if let Some(label) = classify_non_success_log(&config.process, &listener_log_text) {
            kill_child(&mut joiner);
            bail!("local listening peer reported {label}");
        }
        if let Some(label) = classify_non_success_log(&config.process, &joiner_log_text) {
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
            bail!("local listening peer exited before pass marker: {status}");
        }
        if let Some(status) = joiner_exited {
            kill_child(&mut joiner);
            bail!("local joining peer exited before pass marker: {status}");
        }

        if started.elapsed() > timeout {
            kill_child(&mut joiner);
            bail!(
                "timed out after {}s waiting for both local peers to reach a pass marker",
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
    let raw_addr = wait_for_join_addr(config, listener, listener_log, local_ip, timeout)?;
    let join_addr = rewrite_join_addr(&raw_addr, local_ip)?;
    println!("{}{}", config.webrtc.join_addr_marker, join_addr);
    let mut args = joiner_args(config, &join_addr);
    args.extend(remote.join_args.clone());
    ssh::launch_remote(config, remote, &args, false)?;

    wait_for_remote_pass(&config.process, listener, listener_log, timeout, || {
        ssh::remote_log(remote)
    })
}

fn wait_for_remote_pass(
    process: &crate::config::ProcessConfig,
    listener: &mut Child,
    listener_log: &Path,
    timeout: Duration,
    mut remote_log: impl FnMut() -> Result<String>,
) -> Result<String> {
    let started = Instant::now();
    loop {
        check_interrupted()?;
        let listener_log_text = read_lossy(listener_log);
        if let Some(LogSignal::Fatal(label)) = classify_log(process, &listener_log_text) {
            bail!("local listening peer reported {label}");
        }

        let remote_log = remote_log()?;
        match (
            classify_log(process, &listener_log_text),
            classify_log(process, &remote_log),
        ) {
            (Some(LogSignal::Pass(label)), Some(LogSignal::Pass(_))) => return Ok(label.into()),
            (_, Some(LogSignal::Fatal(label))) => bail!("remote joining peer reported {label}"),
            _ => {}
        }

        if let Some(status) = listener
            .try_wait()
            .context("failed to poll local listening peer")?
        {
            bail!("local listening peer exited before both pass markers in remote mode: {status}");
        }

        if started.elapsed() > timeout {
            bail!(
                "timed out after {}s waiting for pass marker",
                timeout.as_secs()
            );
        }

        thread::sleep(Duration::from_secs(1));
    }
}

fn launch_listener(config: &Config, port: u16, log_path: &Path) -> Result<Child> {
    let args = listener_args(config, port);
    spawn_logged(config, &args, log_path, "Local listening peer")
}

fn wait_for_join_addr(
    config: &Config,
    listener: &mut Child,
    log_path: &Path,
    preferred_ip: IpAddr,
    timeout: Duration,
) -> Result<String> {
    let started = Instant::now();
    loop {
        check_interrupted()?;
        let text = read_lossy(log_path);
        if let Some(addr) =
            parse_join_addr_prefer_ip(&text, &config.webrtc.join_addr_marker, Some(preferred_ip))
        {
            return Ok(addr);
        }
        if let Some(LogSignal::Fatal(label)) = classify_log(&config.process, &text) {
            bail!("local listening peer reported {label} before WebRTC address was emitted");
        }
        if let Some(status) = listener
            .try_wait()
            .context("failed to poll local listening peer while waiting for WebRTC address")?
        {
            bail!("local listening peer exited before WebRTC address was emitted: {status}");
        }
        if started.elapsed() > timeout {
            bail!(
                "timed out after {}s waiting for {}",
                timeout.as_secs(),
                config.webrtc.join_addr_marker
            );
        }
        thread::sleep(Duration::from_millis(250));
    }
}

// =============================================================================
// ANDROID 1v1 (desktop listener + Android-emulator joiner)
// =============================================================================

pub struct AndroidRunOptions {
    pub config_path: PathBuf,
    pub max_runs: Option<u32>,
    pub timeout_secs: Option<u64>,
    pub stop_on_failure: bool,
    pub no_build: bool,
    pub no_install: bool,
    pub local_ip: Option<IpAddr>,
    pub webrtc_port: Option<u16>,
    pub dry_run: bool,
    pub adb_serial: Option<String>,
    pub device: bool,
    pub launch_config: Option<String>,
    pub output_format: OutputFormat,
}

pub struct AndroidUiRunOptions {
    pub config_path: PathBuf,
    pub max_runs: Option<u32>,
    pub timeout_secs: Option<u64>,
    pub stop_on_failure: bool,
    pub no_build: bool,
    pub no_install: bool,
    pub dry_run: bool,
    pub adb_serial: Option<String>,
    pub device: bool,
    pub launch_config: Option<String>,
    pub output_format: OutputFormat,
}

pub struct AndroidDoctorOptions {
    pub config_path: PathBuf,
    pub adb_serial: Option<String>,
    pub device: bool,
    pub local_ip: Option<IpAddr>,
    pub dry_run: bool,
}

struct AndroidOneRunOptions<'a> {
    port: u16,
    local_ip: IpAddr,
    timeout: Duration,
    dry_run: bool,
    launch_config: Option<&'a str>,
}

struct AndroidTargetGuard<'a> {
    cfg: &'a AndroidConfig,
    emulator: Option<Child>,
}

impl<'a> AndroidTargetGuard<'a> {
    fn new(cfg: &'a AndroidConfig, emulator: Option<Child>) -> Self {
        Self { cfg, emulator }
    }
}

impl Drop for AndroidTargetGuard<'_> {
    fn drop(&mut self) {
        android::kill_emulator(self.cfg, self.emulator.take());
    }
}

pub fn run_android_1v1(options: AndroidRunOptions) -> Result<()> {
    let config = load_config(&options.config_path)?;
    println!("Fragpipe project: {}", config.game.name);
    let mut android_cfg = config
        .android
        .as_ref()
        .context("[android] section is required for android-1v1; see fragpipe README")?
        .clone();
    apply_android_overrides(&mut android_cfg, options.adb_serial, options.device);
    resolve_android_host_paths(&config, &mut android_cfg);

    let max_runs = options.max_runs.unwrap_or(config.webrtc.max_runs);
    validate_max_runs(max_runs)?;
    let timeout = Duration::from_secs(options.timeout_secs.unwrap_or(config.webrtc.timeout_secs));
    let port = options.webrtc_port.unwrap_or(config.webrtc.port);
    let local_ip = android_local_ip(
        options.local_ip,
        &android_cfg,
        config.webrtc.local_ip,
        options.device,
    )?;

    if !options.no_build {
        run_build(&config, options.dry_run)?;
        run_apk_build(&config, &android_cfg, options.dry_run)?;
    }

    // Boot the emulator once for the whole run series — re-booting per run is
    // 30-60s of overhead. The APK is reinstalled and app data is cleared for
    // each run so a prior game cannot leak state into the next one.
    let emulator = android::prepare_target(&android_cfg, options.dry_run)?;
    let _target_guard = AndroidTargetGuard::new(&android_cfg, emulator);
    let artifact_root = android_artifact_root(&config, "android-1v1");

    let mut passed = 0;
    let mut failed = 0;
    for run in 1..=max_runs {
        println!("=== ANDROID RUN {run}/{max_runs} ===");
        let report = prepare_android_app_run(&android_cfg, options.no_install, options.dry_run)
            .and_then(|()| {
                run_one_android(
                    &config,
                    &android_cfg,
                    &artifact_root,
                    run,
                    AndroidOneRunOptions {
                        port,
                        local_ip,
                        timeout,
                        dry_run: options.dry_run,
                        launch_config: options.launch_config.as_deref(),
                    },
                )
            });
        let report = match report {
            Ok(r) => r,
            Err(e) => {
                let report = RunReport {
                    run,
                    status: RunStatus::Fail,
                    label: e.to_string(),
                    duration_secs: 0,
                };
                write_android_artifacts(
                    &android_cfg,
                    &artifact_root,
                    run,
                    &report,
                    &[],
                    options.dry_run,
                )?;
                report
            }
        };
        emit_report(options.output_format, &report)?;
        match report.status {
            RunStatus::Pass => passed += 1,
            RunStatus::Fail | RunStatus::Timeout => {
                failed += 1;
                if options.stop_on_failure {
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
        bail!("{failed} android 1v1 run(s) failed")
    }
}

pub fn run_android_ui(options: AndroidUiRunOptions) -> Result<()> {
    let config = load_config(&options.config_path)?;
    println!("Fragpipe project: {}", config.game.name);
    let mut android_cfg = config
        .android
        .as_ref()
        .context("[android] section is required for android-ui; see fragpipe README")?
        .clone();
    apply_android_overrides(&mut android_cfg, options.adb_serial, options.device);
    android_cfg = android_ui_config(android_cfg);
    resolve_android_host_paths(&config, &mut android_cfg);

    let max_runs = options.max_runs.unwrap_or(config.webrtc.max_runs);
    validate_max_runs(max_runs)?;
    let timeout = Duration::from_secs(options.timeout_secs.unwrap_or(60));

    if !options.no_build {
        run_apk_build(&config, &android_cfg, options.dry_run)?;
    }

    let emulator = android::prepare_target(&android_cfg, options.dry_run)?;
    let _target_guard = AndroidTargetGuard::new(&android_cfg, emulator);
    let artifact_root = android_artifact_root(&config, "android-ui");

    let mut passed = 0;
    let mut failed = 0;
    for run in 1..=max_runs {
        println!("=== ANDROID UI RUN {run}/{max_runs} ===");
        let report = prepare_android_app_run(&android_cfg, options.no_install, options.dry_run)
            .and_then(|()| {
                run_one_android_ui(
                    &config,
                    &android_cfg,
                    &artifact_root,
                    run,
                    timeout,
                    options.dry_run,
                    options.launch_config.as_deref(),
                )
            });
        let report = match report {
            Ok(report) => report,
            Err(error) => {
                let report = RunReport {
                    run,
                    status: RunStatus::Fail,
                    label: error.to_string(),
                    duration_secs: 0,
                };
                write_android_artifacts(
                    &android_cfg,
                    &artifact_root,
                    run,
                    &report,
                    &[],
                    options.dry_run,
                )?;
                report
            }
        };
        emit_report(options.output_format, &report)?;
        match report.status {
            RunStatus::Pass => passed += 1,
            RunStatus::Fail | RunStatus::Timeout => {
                failed += 1;
                if options.stop_on_failure {
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
        bail!("{failed} android UI run(s) failed")
    }
}

// =============================================================================
// SHIP (standalone build + deploy)
// =============================================================================

pub struct ShipOptions {
    pub config_path: PathBuf,
    pub no_build: bool,
    pub no_deploy: bool,
    pub remote: String,
    pub restart: bool,
    pub launch_args: Option<Vec<String>>,
    pub dry_run: bool,
}

pub fn run_ship(options: ShipOptions) -> Result<()> {
    let config = load_config(&options.config_path)?;
    println!("Fragpipe project: {}", config.game.name);
    let remote = select_remote(&config, &options.remote)?;

    if !options.no_build {
        println!("==> Building...");
        run_build(&config, options.dry_run)?;
    }

    if !options.no_deploy {
        println!("==> Deploying to {} ({})...", remote.name, remote.host);
        ssh::deploy(&config, remote, options.dry_run)?;
    }

    if options.restart {
        println!("==> Restarting on {}...", remote.name);
        ssh::stop_remote(&config, remote, options.dry_run)?;
        let args = options
            .launch_args
            .as_ref()
            .cloned()
            .unwrap_or_else(|| remote.join_args.clone());
        ssh::launch_remote(&config, remote, &args, options.dry_run)?;
        println!("==> {} restarted successfully", remote.name);
    }

    println!("==> ship complete");
    Ok(())
}

pub fn run_android_doctor(options: AndroidDoctorOptions) -> Result<()> {
    let config = load_config(&options.config_path)?;
    let mut android_cfg = config
        .android
        .as_ref()
        .context("[android] section is required for android-doctor; see fragpipe README")?
        .clone();
    apply_android_overrides(&mut android_cfg, options.adb_serial, options.device);
    resolve_android_host_paths(&config, &mut android_cfg);
    let local_ip = android_local_ip(
        options.local_ip,
        &android_cfg,
        config.webrtc.local_ip,
        options.device,
    )?;

    println!("=== ANDROID DOCTOR ===");
    let ui_cfg = android_ui_config(android_cfg.clone());
    let apk = &android_cfg.apk_path;
    let ui_apk = &ui_cfg.apk_path;
    println!("target: {:?}", android_cfg.target);
    println!("desktop address: {local_ip}");
    println!("package: {}", android_cfg.package_name);
    println!("activity: {}", android_cfg.activity_name);
    println!("apk: {}", apk.display());
    validate_android_identity(&android_cfg)?;
    if !apk.exists() {
        bail!("configured APK does not exist: {}", apk.display());
    }
    android::validate_apk_manifest(&android_cfg, options.dry_run)?;
    if ui_apk != apk
        || ui_cfg.package_name != android_cfg.package_name
        || ui_cfg.activity_name != android_cfg.activity_name
    {
        println!("ui package: {}", ui_cfg.package_name);
        println!("ui activity: {}", ui_cfg.activity_name);
        println!("ui apk: {}", ui_apk.display());
        validate_android_identity(&ui_cfg)?;
        if !ui_apk.exists() {
            bail!("configured UI APK does not exist: {}", ui_apk.display());
        }
        android::validate_apk_manifest(&ui_cfg, options.dry_run)?;
    }
    android::check_adb(&android_cfg, options.dry_run)?;
    if android_cfg.target == AndroidTarget::Emulator {
        let emulator = android::emulator_bin(&android_cfg)?;
        println!("emulator: {}", emulator.display());
        if !emulator.is_file() {
            bail!(
                "configured emulator binary does not exist: {}; fix the Android SDK composition or [android].emulator_bin",
                emulator.display()
            );
        }
        if !options.dry_run {
            let avds = android::list_avds(&android_cfg)?;
            if !avds.iter().any(|avd| avd == &android_cfg.avd_name) {
                bail!(
                    "configured AVD `{}` is not available; emulator -list-avds returned: {}",
                    android_cfg.avd_name,
                    if avds.is_empty() {
                        "<none>".to_string()
                    } else {
                        avds.join(", ")
                    }
                );
            }
        }
        println!("avd: {}", android_cfg.avd_name);
    } else if android_cfg.adb_serial.is_none() {
        bail!("device target requires adb_serial or --adb-serial");
    } else if !options.dry_run {
        android::check_device(&android_cfg)?;
    }
    println!("android doctor passed");
    Ok(())
}

fn validate_android_identity(cfg: &AndroidConfig) -> Result<()> {
    if cfg.package_name.trim().is_empty() || cfg.activity_name.trim().is_empty() {
        bail!("Android package_name and activity_name must both be non-empty");
    }
    Ok(())
}

fn run_one_android(
    config: &Config,
    android_cfg: &crate::config::AndroidConfig,
    artifact_root: &Path,
    run: u32,
    options: AndroidOneRunOptions<'_>,
) -> Result<RunReport> {
    let started = Instant::now();
    let listener_log = config.game.listener_log.clone();
    if options.dry_run {
        let args = listener_args(config, options.port);
        println!(
            "==> Local listening peer: {}",
            command_line(&config.game.binary, &args)
        );
        let join_addr = android_dry_run_join_addr(options.local_ip, options.port)?;
        println!("{}{}", config.webrtc.join_addr_marker, join_addr);
        if let Some(contents) = options.launch_config {
            android::push_launch_config(android_cfg, contents, true)?;
        }
        android::push_rendezvous(android_cfg, &join_addr, true)?;
        android::tail_logcat(android_cfg, true)?;
        android::start_activity(android_cfg, true)?;
        let report = report_from_result(run, started, Ok("DRY_RUN".into()));
        write_android_artifacts(android_cfg, artifact_root, run, &report, &[], true)?;
        return Ok(report);
    }
    if !options.dry_run {
        remove_if_exists(&listener_log)?;
        remove_if_exists(&android_cfg.logcat_log)?;
    }

    let mut listener = launch_listener(config, options.port, &listener_log)?;
    let mut logcat: Option<Child> = None;

    let result: Result<String> = (|| {
        let raw_addr = if options.dry_run {
            "/ip4/127.0.0.1/udp/27200/webrtc-direct/certhash/uEiDryRunCerthash".to_string()
        } else {
            wait_for_join_addr(
                config,
                &mut listener,
                &listener_log,
                options.local_ip,
                options.timeout,
            )?
        };
        let join_addr = if options.dry_run {
            raw_addr
        } else {
            rewrite_join_addr(&raw_addr, options.local_ip)?
        };
        println!("{}{}", config.webrtc.join_addr_marker, join_addr);

        if let Some(contents) = options.launch_config {
            android::push_launch_config(android_cfg, contents, options.dry_run)?;
        }
        android::push_rendezvous(android_cfg, &join_addr, options.dry_run)?;
        logcat = android::tail_logcat(android_cfg, options.dry_run)?;
        android::start_activity(android_cfg, options.dry_run)?;

        if options.dry_run {
            return Ok("DRY_RUN".into());
        }

        wait_for_pass(
            &config.process,
            &listener_log,
            &android_cfg.logcat_log,
            options.timeout,
            &mut listener,
        )
    })();

    // Tear-down: stop activity, kill logcat tail, kill listener.
    if !options.dry_run {
        let _ = android::force_stop(android_cfg);
    }
    if let Some(mut child) = logcat.take() {
        kill_child(&mut child);
    }
    kill_child(&mut listener);

    let report = report_from_result(run, started, result);
    write_android_artifacts(
        android_cfg,
        artifact_root,
        run,
        &report,
        &[
            (&listener_log, "desktop-listening-peer.log"),
            (&android_cfg.logcat_log, "android-logcat.log"),
        ],
        options.dry_run,
    )?;
    Ok(report)
}

fn android_dry_run_join_addr(local_ip: IpAddr, port: u16) -> Result<String> {
    let raw_addr = format!("/ip4/0.0.0.0/udp/{port}/webrtc-direct/certhash/uEiDryRunCerthash");
    rewrite_join_addr(&raw_addr, local_ip)
}

fn run_one_android_ui(
    config: &Config,
    android_cfg: &AndroidConfig,
    artifact_root: &Path,
    run: u32,
    timeout: Duration,
    dry_run: bool,
    launch_config: Option<&str>,
) -> Result<RunReport> {
    let started = Instant::now();
    if !dry_run {
        remove_if_exists(&android_cfg.logcat_log)?;
    }
    let mut logcat: Option<Child> = None;
    let screenshot_path = android_cfg.screenshot_dir.join(format!("run-{run}.png"));

    let result: Result<String> = (|| {
        if let Some(contents) = launch_config {
            android::push_launch_config(android_cfg, contents, dry_run)?;
        }
        logcat = android::tail_logcat(android_cfg, dry_run)?;
        android::start_activity(android_cfg, dry_run)?;
        if dry_run {
            return Ok("DRY_RUN".into());
        }

        let started = Instant::now();
        let mut next_probe = Duration::from_secs(5);
        let mut last_probe_error = None;
        loop {
            check_interrupted()?;
            let logcat_text = read_lossy(&android_cfg.logcat_log);
            if let Some(label) = classify_non_success_log(&config.process, &logcat_text) {
                bail!("android UI app reported {label}");
            }
            let elapsed = started.elapsed();
            if elapsed >= next_probe {
                let probe = android::ensure_package_foreground(android_cfg).and_then(|()| {
                    android::capture_screenshot(android_cfg, &screenshot_path, false)
                });
                match probe {
                    Ok(()) => return Ok("LANDSCAPE_NONUNIFORM_SCREENSHOT".into()),
                    Err(error) => {
                        eprintln!("[android] UI readiness probe failed: {error:#}");
                        last_probe_error = Some(error.to_string());
                        next_probe = elapsed.saturating_add(Duration::from_secs(1));
                    }
                }
            }
            if elapsed >= timeout {
                bail!(
                    "timed out after {}s waiting for android UI screenshot; last readiness error: {}",
                    timeout.as_secs(),
                    last_probe_error
                        .as_deref()
                        .unwrap_or("no readiness probe completed")
                );
            }
            thread::sleep(Duration::from_millis(500));
        }
    })();

    if !dry_run {
        let _ = android::force_stop(android_cfg);
    }
    if let Some(mut child) = logcat.take() {
        kill_child(&mut child);
    }

    let report = report_from_result(run, started, result);
    write_android_artifacts(
        android_cfg,
        artifact_root,
        run,
        &report,
        &[
            (&android_cfg.logcat_log, "android-logcat.log"),
            (&screenshot_path, "screenshot.png"),
        ],
        dry_run,
    )?;
    Ok(report)
}

fn apply_android_overrides(cfg: &mut AndroidConfig, adb_serial: Option<String>, device: bool) {
    if device {
        cfg.target = AndroidTarget::Device;
    }
    if adb_serial.is_some() {
        cfg.adb_serial = adb_serial;
    }
}

fn android_local_ip(
    cli_override: Option<IpAddr>,
    cfg: &AndroidConfig,
    webrtc_default: IpAddr,
    device_override: bool,
) -> Result<IpAddr> {
    if device_override && cli_override.is_none() {
        bail!("--device requires --local-ip with a desktop address reachable from the device");
    }
    let ip = match (cli_override, cfg.local_ip) {
        (Some(ip), _) => ip,
        (None, Some(ip)) => ip,
        (None, None) if cfg.target == AndroidTarget::Device => bail!(
            "physical-device mode requires --local-ip or [android].local_ip with a reachable desktop address"
        ),
        (None, None) => webrtc_default,
    };
    if cfg.target == AndroidTarget::Device {
        validate_physical_device_ip(ip)?;
    }
    Ok(ip)
}

fn validate_physical_device_ip(ip: IpAddr) -> Result<()> {
    let invalid_class = ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_multicast()
        || match ip {
            IpAddr::V4(ip) => ip.is_link_local() || ip.is_broadcast(),
            IpAddr::V6(ip) => ip.is_unicast_link_local(),
        };
    let emulator_alias = ip == IpAddr::V4(std::net::Ipv4Addr::new(10, 0, 2, 2));
    if invalid_class || emulator_alias {
        bail!(
            "physical-device local IP {ip} is not a usable desktop address; use a non-loopback, non-link-local unicast address reachable from the device (10.0.2.2 is emulator-only)"
        );
    }
    Ok(())
}

fn resolve_android_host_paths(config: &Config, cfg: &mut AndroidConfig) {
    let root = project_root(config);
    cfg.apk_path = resolve_path(root, &cfg.apk_path);
    cfg.ui_apk_path = cfg
        .ui_apk_path
        .as_ref()
        .map(|path| resolve_path(root, path));
    cfg.logcat_log = resolve_path(root, &cfg.logcat_log);
    cfg.screenshot_dir = resolve_path(root, &cfg.screenshot_dir);
}

fn prepare_android_app_run(
    android_cfg: &AndroidConfig,
    no_install: bool,
    dry_run: bool,
) -> Result<()> {
    if !dry_run {
        android::force_stop(android_cfg)?;
    }
    if !no_install {
        if !dry_run && !android_cfg.apk_path.is_file() {
            bail!(
                "Android APK is missing before install: {}; fix the configured APK build command and validate its declared output",
                android_cfg.apk_path.display()
            );
        }
        android::install_apk(android_cfg, dry_run)?;
    }
    android::clear_app_data(android_cfg, dry_run)
}

fn run_apk_build(config: &Config, android_cfg: &AndroidConfig, dry_run: bool) -> Result<()> {
    if let Some(command) = apk_build_command(android_cfg) {
        run_shell_command("Android APK build", command, project_root(config), dry_run)?;
    }
    Ok(())
}

fn apk_build_command(android_cfg: &AndroidConfig) -> Option<&str> {
    match android_cfg.target {
        AndroidTarget::Device => android_cfg
            .device_apk_build_command
            .as_deref()
            .or(android_cfg.apk_build_command.as_deref()),
        AndroidTarget::Emulator => android_cfg.apk_build_command.as_deref(),
    }
}

fn android_ui_config(mut cfg: AndroidConfig) -> AndroidConfig {
    if let Some(path) = cfg.ui_apk_path.clone() {
        cfg.apk_path = path;
    }
    if let Some(package) = cfg.ui_package_name.clone() {
        cfg.package_name = package;
    }
    if let Some(activity) = cfg.ui_activity_name.clone() {
        cfg.activity_name = activity;
    }
    if let Some(tag) = cfg.ui_log_tag.clone() {
        cfg.log_tag = tag;
    }
    if let Some(command) = cfg.ui_apk_build_command.clone() {
        cfg.apk_build_command = Some(command);
    }
    if let Some(command) = cfg.device_ui_apk_build_command.clone() {
        cfg.device_apk_build_command = Some(command);
    }
    cfg
}

fn android_artifact_root(config: &Config, mode: &str) -> PathBuf {
    project_root(config)
        .join("logs")
        .join("fragpipe")
        .join(format!("{}_{}", timestamp_secs(), mode))
}

fn report_from_result(run: u32, started: Instant, result: Result<String>) -> RunReport {
    let duration_secs = started.elapsed().as_secs();
    match result {
        Ok(label) => RunReport {
            run,
            status: RunStatus::Pass,
            label,
            duration_secs,
        },
        Err(error) if error.to_string().contains("timed out") => RunReport {
            run,
            status: RunStatus::Timeout,
            label: error.to_string(),
            duration_secs,
        },
        Err(error) => RunReport {
            run,
            status: RunStatus::Fail,
            label: error.to_string(),
            duration_secs,
        },
    }
}

fn write_android_artifacts(
    android_cfg: &AndroidConfig,
    root: &Path,
    run: u32,
    report: &RunReport,
    files: &[(&Path, &str)],
    dry_run: bool,
) -> Result<()> {
    if dry_run {
        println!("artifacts: skipped (dry run)");
        return Ok(());
    }
    let run_dir = root.join(format!("run-{run}"));
    fs::create_dir_all(&run_dir)
        .with_context(|| format!("failed to create artifact dir {}", run_dir.display()))?;
    for (src, name) in files {
        if src.exists() {
            fs::copy(src, run_dir.join(name)).with_context(|| {
                format!(
                    "failed to copy Android evidence {} to {}",
                    src.display(),
                    run_dir.join(name).display()
                )
            })?;
        } else if report.status == RunStatus::Pass {
            bail!(
                "Android run passed without required evidence file {}",
                src.display()
            );
        }
    }
    fs::write(
        run_dir.join("report.json"),
        serde_json::to_string_pretty(report)?,
    )
    .with_context(|| format!("failed to write {}", run_dir.join("report.json").display()))?;
    fs::write(
        run_dir.join("target.txt"),
        format!(
            "target={:?}\npackage={}\nactivity={}\nadb_serial={:?}\n",
            android_cfg.target,
            android_cfg.package_name,
            android_cfg.activity_name,
            android_cfg.adb_serial
        ),
    )
    .with_context(|| format!("failed to write {}", run_dir.join("target.txt").display()))?;
    println!("artifacts: {}", run_dir.display());
    Ok(())
}

fn timestamp_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn validate_max_runs(max_runs: u32) -> Result<()> {
    if max_runs == 0 {
        bail!("max_runs must be at least 1");
    }
    Ok(())
}

/// Watch both peer logs until both report `Pass` or one reports `Fatal`.
/// Identical detection logic to `run_one_local_inner`, factored out.
fn wait_for_pass(
    process: &crate::config::ProcessConfig,
    listener_log: &Path,
    joiner_log: &Path,
    timeout: Duration,
    listener: &mut Child,
) -> Result<String> {
    let started = Instant::now();
    loop {
        check_interrupted()?;
        let listener_text = read_lossy(listener_log);
        let joiner_text = read_lossy(joiner_log);

        if let Some(LogSignal::Pass(label)) = classify_log(process, &listener_text)
            && matches!(
                classify_log(process, &joiner_text),
                Some(LogSignal::Pass(_))
            )
        {
            return Ok(label.into());
        }

        if let Some(label) = classify_non_success_log(process, &listener_text) {
            bail!("desktop listening peer reported {label}");
        }
        if let Some(label) = classify_non_success_log(process, &joiner_text) {
            bail!("android joining peer reported {label}");
        }

        if let Some(status) = listener
            .try_wait()
            .context("failed to poll desktop listening peer")?
        {
            bail!("desktop listening peer exited before pass marker: {status}");
        }

        if started.elapsed() > timeout {
            bail!(
                "timed out after {}s waiting for both peers to reach a pass marker",
                timeout.as_secs()
            );
        }

        thread::sleep(Duration::from_secs(1));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_without_remote_is_valid() {
        let config: Config = toml::from_str(
            r#"
            [game]
            binary = "target/release/game"

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
        assert_eq!(config.internet.max_runs, 1);
        assert_eq!(config.internet.timeout_secs, 200);
        assert_eq!(config.internet.pass_marker, "GAME OVER");
    }

    #[test]
    fn join_address_wait_fails_immediately_when_listener_exits() {
        let config: Config = toml::from_str(
            r#"
            [game]
            binary = "target/release/game"

            [webrtc]
            local_ip = "127.0.0.1"
            "#,
        )
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let listener_log = dir.path().join("listener.log");
        std::fs::write(&listener_log, "listen failed: address already in use\n").unwrap();
        let mut listener = Command::new("sh").args(["-c", "exit 23"]).spawn().unwrap();

        let started = Instant::now();
        let error = wait_for_join_addr(
            &config,
            &mut listener,
            &listener_log,
            "127.0.0.1".parse().unwrap(),
            Duration::from_secs(5),
        )
        .unwrap_err();

        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(
            error
                .to_string()
                .contains("local listening peer exited before WebRTC address was emitted")
        );
    }

    #[test]
    fn first_marker_line_finds_matching_line() {
        let output = "line1\nPASS: all good\nline3\n";
        assert_eq!(first_marker_line(output, "PASS:"), Some("PASS: all good"));
    }

    #[test]
    fn first_marker_line_returns_none_when_not_found() {
        let output = "line1\nline2\n";
        assert_eq!(first_marker_line(output, "PASS:"), None);
    }

    #[test]
    fn first_marker_line_finds_last_match() {
        let output = "PASS: first\nsome log\nPASS: second";
        assert_eq!(first_marker_line(output, "PASS:"), Some("PASS: first"));
    }

    #[test]
    fn internet_log_excerpt_empty_log_returns_empty() {
        assert_eq!(internet_log_excerpt(""), "");
    }

    #[test]
    fn internet_log_excerpt_returns_at_most_eight_lines() {
        let lines: Vec<String> = (1..=20).map(|i| format!("line {i}")).collect();
        let output = lines.join("\n");
        let excerpt = internet_log_excerpt(&output);
        assert!(excerpt.starts_with("; log tail:"));
        assert_eq!(excerpt.matches('\n').count(), 8);
        assert!(excerpt.contains("line 13"));
        assert!(excerpt.contains("line 20"));
    }

    #[test]
    fn internet_log_excerpt_less_than_eight_lines() {
        let output = "line1\nline2\n";
        let excerpt = internet_log_excerpt(output);
        assert_eq!(excerpt.matches('\n').count(), 2);
    }

    #[test]
    fn internet_dry_run_does_not_create_log_directory() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("must-not-exist");
        let label = run_one_internet_inner(&InternetOneRunOptions {
            run: 1,
            script: Path::new("/path/that/must/not/be/executed/script"),
            workdir: dir.path(),
            game_bin: Path::new("/path/that/must/not/be/executed/game"),
            rdv_bin: Path::new("/path/that/must/not/be/executed/rdv"),
            asset_root: dir.path(),
            log_dir: &log_dir,
            timeout: Duration::from_secs(1),
            timeout_secs: 1,
            pass_marker: "PASS",
            dry_run: true,
        })
        .unwrap();

        assert_eq!(label, "DRY_RUN");
        assert!(!log_dir.exists());
    }

    #[test]
    fn timestamp_secs_is_positive() {
        assert!(timestamp_secs() > 1700000000);
    }

    #[test]
    fn zero_max_runs_is_rejected() {
        assert!(
            validate_max_runs(0)
                .unwrap_err()
                .to_string()
                .contains("at least 1")
        );
        validate_max_runs(1).unwrap();
    }

    #[test]
    fn emit_report_text_format_does_not_error() {
        let report = RunReport {
            run: 1,
            status: RunStatus::Pass,
            label: "test".into(),
            duration_secs: 5,
        };
        emit_report(OutputFormat::Text, &report).unwrap();
    }

    #[test]
    fn emit_report_jsonl_format_does_not_error() {
        let report = RunReport {
            run: 1,
            status: RunStatus::Pass,
            label: "test".into(),
            duration_secs: 5,
        };
        emit_report(OutputFormat::Jsonl, &report).unwrap();
    }

    #[test]
    fn apply_android_overrides_sets_device_flag() {
        let mut cfg = create_test_android_cfg();
        assert_eq!(cfg.target, AndroidTarget::Emulator);
        apply_android_overrides(&mut cfg, None, true);
        assert_eq!(cfg.target, AndroidTarget::Device);
    }

    #[test]
    fn apply_android_overrides_sets_adb_serial() {
        let mut cfg = create_test_android_cfg();
        assert!(cfg.adb_serial.is_none());
        apply_android_overrides(&mut cfg, Some("my-serial".into()), false);
        assert_eq!(cfg.adb_serial, Some("my-serial".into()));
    }

    #[test]
    fn apply_android_overrides_no_changes_when_no_overrides() {
        let mut cfg = create_test_android_cfg();
        let original_target = cfg.target;
        let original_serial = cfg.adb_serial.clone();
        apply_android_overrides(&mut cfg, None, false);
        assert_eq!(cfg.target, original_target);
        assert_eq!(cfg.adb_serial, original_serial);
    }

    #[test]
    fn android_local_ip_priority_is_cli_then_android_then_webrtc() {
        let mut cfg = create_test_android_cfg();
        let webrtc: IpAddr = "127.0.0.1".parse().unwrap();
        assert_eq!(android_local_ip(None, &cfg, webrtc, false).unwrap(), webrtc);

        cfg.local_ip = Some("10.0.2.2".parse().unwrap());
        assert_eq!(
            android_local_ip(None, &cfg, webrtc, false).unwrap(),
            "10.0.2.2".parse::<IpAddr>().unwrap()
        );
        assert_eq!(
            android_local_ip(Some("192.0.2.20".parse().unwrap()), &cfg, webrtc, false).unwrap(),
            "192.0.2.20".parse::<IpAddr>().unwrap()
        );
    }

    #[test]
    fn device_override_requires_explicit_cli_local_ip() {
        let mut cfg = create_test_android_cfg();
        cfg.target = AndroidTarget::Device;
        cfg.local_ip = Some("10.0.2.2".parse().unwrap());
        let error = android_local_ip(None, &cfg, "127.0.0.1".parse().unwrap(), true).unwrap_err();
        assert!(error.to_string().contains("--device requires --local-ip"));
    }

    #[test]
    fn configured_device_requires_android_local_ip() {
        let mut cfg = create_test_android_cfg();
        cfg.target = AndroidTarget::Device;
        let error = android_local_ip(None, &cfg, "127.0.0.1".parse().unwrap(), false).unwrap_err();
        assert!(error.to_string().contains("physical-device mode requires"));
    }

    #[test]
    fn physical_device_rejects_non_routable_and_emulator_only_addresses() {
        for address in [
            "0.0.0.0",
            "127.0.0.1",
            "169.254.1.1",
            "224.0.0.1",
            "255.255.255.255",
            "10.0.2.2",
            "::",
            "::1",
            "fe80::1",
            "ff02::1",
        ] {
            let error = validate_physical_device_ip(address.parse().unwrap()).unwrap_err();
            assert!(
                error.to_string().contains("not a usable desktop address"),
                "unexpected error for {address}: {error:#}"
            );
        }
        validate_physical_device_ip("192.168.1.20".parse().unwrap()).unwrap();
    }

    #[test]
    fn device_build_command_overrides_generic_command() {
        let mut cfg = create_test_android_cfg();
        cfg.apk_build_command = Some("generic-build".into());
        assert_eq!(apk_build_command(&cfg), Some("generic-build"));
        cfg.target = AndroidTarget::Device;
        assert_eq!(apk_build_command(&cfg), Some("generic-build"));
        cfg.device_apk_build_command = Some("device-build".into());
        assert_eq!(apk_build_command(&cfg), Some("device-build"));
    }

    #[cfg(unix)]
    #[test]
    fn every_prepared_android_run_reinstalls_and_clears_app_data() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().unwrap();
        let adb_log = dir.path().join("adb.log");
        let adb = dir.path().join("adb");
        std::fs::write(
            &adb,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {}\ncase \"$*\" in *\"pm clear\"*|*\"install -r -t\"*) echo Success;; esac\n",
                adb_log.display()
            ),
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&adb).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&adb, permissions).unwrap();

        let mut cfg = create_test_android_cfg();
        cfg.target = AndroidTarget::Device;
        cfg.adb_bin = Some(adb);
        cfg.apk_path = dir.path().join("app.apk");
        std::fs::write(&cfg.apk_path, b"apk fixture").unwrap();
        prepare_android_app_run(&cfg, false, false).unwrap();
        prepare_android_app_run(&cfg, false, false).unwrap();

        let invocations = std::fs::read_to_string(adb_log).unwrap();
        assert_eq!(invocations.matches("install -r -t").count(), 2);
        assert_eq!(invocations.matches("shell pm clear com.test").count(), 2);
    }

    #[test]
    fn android_ui_config_preserves_fields_without_ui_overrides() {
        let cfg = create_test_android_cfg();
        let ui_cfg = android_ui_config(cfg.clone());
        assert_eq!(ui_cfg.apk_path, cfg.apk_path);
        assert_eq!(ui_cfg.package_name, cfg.package_name);
        assert_eq!(ui_cfg.activity_name, cfg.activity_name);
    }

    #[test]
    fn android_ui_config_applies_all_ui_overrides() {
        let cfg = AndroidConfig {
            target: AndroidTarget::Emulator,
            avd_name: "test-avd".into(),
            adb_serial: None,
            local_ip: None,
            apk_path: PathBuf::from("original.apk"),
            apk_build_command: None,
            device_apk_build_command: None,
            ui_apk_path: Some(PathBuf::from("ui.apk")),
            ui_package_name: Some("com.ui.pkg".into()),
            ui_activity_name: Some("UiActivity".into()),
            ui_log_tag: Some("UI_TAG".into()),
            ui_apk_build_command: Some("build-ui".into()),
            device_ui_apk_build_command: Some("build-device-ui".into()),
            package_name: "com.original.pkg".into(),
            activity_name: "OriginalActivity".into(),
            log_tag: "original".into(),
            rendezvous_path: "/data/local/tmp/rendezvous.txt".into(),
            logcat_log: PathBuf::from("fragpipe-android.log"),
            emulator_bin: None,
            adb_bin: None,
            aapt_bin: None,
            emulator_args: vec![],
            boot_timeout_secs: 180,
            launch_config_path: "/data/local/tmp/config.json".into(),
            screenshot_dir: PathBuf::from("screenshots"),
        };
        let ui_cfg = android_ui_config(cfg);
        assert_eq!(ui_cfg.apk_path, PathBuf::from("ui.apk"));
        assert_eq!(ui_cfg.package_name, "com.ui.pkg");
        assert_eq!(ui_cfg.activity_name, "UiActivity");
        assert_eq!(ui_cfg.log_tag, "UI_TAG");
        assert_eq!(ui_cfg.apk_build_command, Some("build-ui".into()));
        assert_eq!(
            ui_cfg.device_apk_build_command,
            Some("build-device-ui".into())
        );
    }

    #[test]
    fn android_1v1_dry_run_preserves_existing_logs_and_creates_no_artifacts() {
        let dir = tempfile::tempdir().unwrap();
        let listener_log = dir.path().join("listener.log");
        let android_log = dir.path().join("android.log");
        std::fs::write(&listener_log, "listener sentinel").unwrap();
        std::fs::write(&android_log, "android sentinel").unwrap();
        let config_path = dir.path().join("fragpipe.toml");
        std::fs::write(
            &config_path,
            format!(
                r#"
                [game]
                binary = "game"
                project_root = {root:?}
                listener_log = {listener:?}

                [webrtc]
                local_ip = "10.0.2.2"
                max_runs = 1

                [android]
                apk_path = "app.apk"
                package_name = "com.example.app"
                adb_bin = "/nonexistent/adb"
                emulator_bin = "/nonexistent/emulator"
                logcat_log = {android:?}
                "#,
                root = dir.path(),
                listener = listener_log,
                android = android_log,
            ),
        )
        .unwrap();

        run_android_1v1(AndroidRunOptions {
            config_path,
            max_runs: Some(1),
            timeout_secs: Some(1),
            stop_on_failure: true,
            no_build: true,
            no_install: true,
            local_ip: None,
            webrtc_port: None,
            dry_run: true,
            adb_serial: None,
            device: false,
            launch_config: Some(r#"{"test":true}"#.into()),
            output_format: OutputFormat::Text,
        })
        .unwrap();

        assert_eq!(
            std::fs::read_to_string(listener_log).unwrap(),
            "listener sentinel"
        );
        assert_eq!(
            std::fs::read_to_string(android_log).unwrap(),
            "android sentinel"
        );
        assert!(!dir.path().join("logs/fragpipe").exists());
    }

    #[test]
    fn android_dry_run_rendezvous_uses_physical_ip_and_non_default_port() {
        let join_addr = android_dry_run_join_addr("192.0.2.10".parse().unwrap(), 38443).unwrap();

        assert_eq!(
            join_addr,
            "/ip4/192.0.2.10/udp/38443/webrtc-direct/certhash/uEiDryRunCerthash"
        );
    }

    #[test]
    fn native_local_dry_run_preserves_existing_logs() {
        let dir = tempfile::tempdir().unwrap();
        let listener_log = dir.path().join("listener.log");
        let joiner_log = dir.path().join("joiner.log");
        std::fs::write(&listener_log, "listener sentinel").unwrap();
        std::fs::write(&joiner_log, "joiner sentinel").unwrap();
        let config: Config = toml::from_str(&format!(
            r#"
            [game]
            binary = "/path/that/must/not/be/executed/game"
            listener_log = {listener:?}
            joiner_log = {joiner:?}

            [webrtc]
            local_ip = "127.0.0.1"
            "#,
            listener = listener_log,
            joiner = joiner_log,
        ))
        .unwrap();

        let label = run_one_local(
            &config,
            27200,
            "127.0.0.1".parse().unwrap(),
            Duration::from_secs(1),
            true,
        )
        .unwrap();

        assert_eq!(label, "DRY_RUN");
        assert_eq!(
            std::fs::read_to_string(listener_log).unwrap(),
            "listener sentinel"
        );
        assert_eq!(
            std::fs::read_to_string(joiner_log).unwrap(),
            "joiner sentinel"
        );
    }

    #[test]
    fn native_remote_dry_run_preserves_existing_listener_log() {
        let dir = tempfile::tempdir().unwrap();
        let listener_log = dir.path().join("listener.log");
        std::fs::write(&listener_log, "listener sentinel").unwrap();
        let config: Config = toml::from_str(&format!(
            r#"
            [game]
            binary = "/path/that/must/not/be/executed/game"
            listener_log = {listener:?}

            [webrtc]
            local_ip = "127.0.0.1"

            [[remote]]
            name = "peer"
            host = "peer.invalid"
            remote_dir = "/tmp/fragpipe"
            "#,
            listener = listener_log,
        ))
        .unwrap();

        let label = run_one_remote(
            &config,
            &config.remote[0],
            27200,
            "127.0.0.1".parse().unwrap(),
            Duration::from_secs(1),
            true,
        )
        .unwrap();

        assert_eq!(label, "DRY_RUN");
        assert_eq!(
            std::fs::read_to_string(listener_log).unwrap(),
            "listener sentinel"
        );
    }

    #[test]
    fn remote_monitor_accepts_accumulated_pass_markers_after_listener_exit() {
        let dir = tempfile::tempdir().unwrap();
        let listener_log = dir.path().join("listener.log");
        std::fs::write(&listener_log, "GAME OVER\n").unwrap();
        let config: Config = toml::from_str(
            r#"
            [game]
            binary = "game"
            "#,
        )
        .unwrap();
        let mut listener = Command::new("sh").args(["-c", "exit 0"]).spawn().unwrap();
        listener.wait().unwrap();

        let label = wait_for_remote_pass(
            &config.process,
            &mut listener,
            &listener_log,
            Duration::from_secs(1),
            || Ok("GAME OVER\n".into()),
        )
        .unwrap();

        assert_eq!(label, "GAME OVER");
    }

    #[test]
    fn report_from_result_ok_maps_to_pass() {
        let result: Result<String> = Ok("all good".into());
        let report = report_from_result(42, Instant::now(), result);
        assert_eq!(report.run, 42);
        assert_eq!(report.status, RunStatus::Pass);
        assert_eq!(report.label, "all good");
    }

    #[test]
    fn report_from_result_timeout_error_maps_to_timeout() {
        let result: Result<String> = Err(anyhow::anyhow!("operation timed out after 300s"));
        let report = report_from_result(1, Instant::now(), result);
        assert_eq!(report.status, RunStatus::Timeout);
    }

    #[test]
    fn report_from_result_other_error_maps_to_fail() {
        let result: Result<String> = Err(anyhow::anyhow!("some other error"));
        let report = report_from_result(1, Instant::now(), result);
        assert_eq!(report.status, RunStatus::Fail);
    }

    #[test]
    fn run_report_serializes_to_json() {
        let report = RunReport {
            run: 3,
            status: RunStatus::Pass,
            label: "test-label".into(),
            duration_secs: 42,
        };
        let json = serde_json::to_string(&report).unwrap();
        assert!(json.contains(r#""run":3"#));
        assert!(json.contains(r#""status":"Pass""#));
        assert!(json.contains(r#""label":"test-label""#));
        assert!(json.contains(r#""duration_secs":42"#));
    }

    #[test]
    fn run_ship_dry_run_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("ship.toml");
        std::fs::write(
            &config_path,
            r#"
            [game]
            binary = "game"
            build_command = "echo build"
            [[remote]]
            name = "test-peer"
            host = "test-host"
            remote_dir = "/remote"
            "#,
        )
        .unwrap();

        // Use the real runner via dispatch (dry_run should not error)
        let options = crate::runner::ShipOptions {
            config_path,
            remote: "test-peer".into(),
            no_build: false,
            no_deploy: false,
            restart: true,
            launch_args: Some(vec!["--join".into(), "addr".into()]),
            dry_run: true,
        };
        // run_ship should succeed in dry-run mode
        crate::runner::run_ship(options).unwrap();
        drop(dir);
    }

    fn create_test_android_cfg() -> AndroidConfig {
        AndroidConfig {
            target: AndroidTarget::Emulator,
            avd_name: "test-avd".into(),
            adb_serial: None,
            local_ip: None,
            apk_path: PathBuf::from("test.apk"),
            apk_build_command: None,
            device_apk_build_command: None,
            ui_apk_path: None,
            ui_package_name: None,
            ui_activity_name: None,
            ui_log_tag: None,
            ui_apk_build_command: None,
            device_ui_apk_build_command: None,
            package_name: "com.test".into(),
            activity_name: "TestActivity".into(),
            log_tag: "test".into(),
            rendezvous_path: "/data/local/tmp/rendezvous.txt".into(),
            logcat_log: PathBuf::from("fragpipe-android.log"),
            emulator_bin: None,
            adb_bin: None,
            aapt_bin: None,
            emulator_args: vec![],
            boot_timeout_secs: 180,
            launch_config_path: "/data/local/tmp/config.json".into(),
            screenshot_dir: PathBuf::from("screenshots"),
        }
    }
}
