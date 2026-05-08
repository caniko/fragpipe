use std::fs;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::process::Child;
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
use crate::process::{kill_child, remove_if_exists, run_build, run_shell_command, spawn_logged};
use crate::ssh;
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

pub fn run_webrtc_1v1(options: WebRtcRunOptions) -> Result<()> {
    let config = load_config(&options.config_path)?;
    println!("Fragpipe project: {}", config.game.name);
    let remote = match options.remote.as_deref() {
        Some(name) => Some(select_remote(&config, name)?),
        None => None,
    };
    let max_runs = options.max_runs.unwrap_or(config.webrtc.max_runs);
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
    remove_if_exists(&listener_log)?;
    remove_if_exists(&joiner_log)?;

    let mut listener = launch_listener(config, port, &listener_log, dry_run)?;
    if dry_run {
        let join_addr =
            format!("/ip4/{local_ip}/udp/{port}/webrtc-direct/certhash/uEiDryRunCerthash");
        let joiner_args = joiner_args(config, &join_addr);
        let mut joiner = spawn_logged(
            config,
            &joiner_args,
            &joiner_log,
            dry_run,
            "Local joining peer",
        )?;
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

    ssh::stop_remote(config, remote, dry_run)?;
    let mut listener = launch_listener(config, port, &listener_log, dry_run)?;
    if dry_run {
        let join_addr =
            format!("/ip4/{local_ip}/udp/{port}/webrtc-direct/certhash/uEiDryRunCerthash");
        let mut args = joiner_args(config, &join_addr);
        args.extend(remote.join_args.clone());
        ssh::launch_remote(config, remote, &args, dry_run)?;
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
    ssh::stop_remote(config, remote, false)?;
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
    let raw_addr = wait_for_join_addr(config, listener_log, local_ip, timeout)?;
    let join_addr = rewrite_join_addr(&raw_addr, local_ip)?;
    println!("{}{}", config.webrtc.join_addr_marker, join_addr);
    let joiner_args = joiner_args(config, &join_addr);
    let mut joiner = spawn_logged(
        config,
        &joiner_args,
        joiner_log,
        false,
        "Local joining peer",
    )?;

    let started = Instant::now();
    let mut listener_exited = None;
    let mut joiner_exited = None;
    loop {
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
    let raw_addr = wait_for_join_addr(config, listener_log, local_ip, timeout)?;
    let join_addr = rewrite_join_addr(&raw_addr, local_ip)?;
    println!("{}{}", config.webrtc.join_addr_marker, join_addr);
    let mut args = joiner_args(config, &join_addr);
    args.extend(remote.join_args.clone());
    ssh::launch_remote(config, remote, &args, false)?;

    let started = Instant::now();
    loop {
        if let Some(status) = listener
            .try_wait()
            .context("failed to poll local listening peer")?
        {
            bail!("local listening peer exited early in remote mode: {status}");
        }

        let listener_log_text = read_lossy(listener_log);
        if let Some(LogSignal::Fatal(label)) = classify_log(&config.process, &listener_log_text) {
            bail!("local listening peer reported {label}");
        }

        let remote_log = ssh::remote_log(remote)?;
        match (
            classify_log(&config.process, &listener_log_text),
            classify_log(&config.process, &remote_log),
        ) {
            (Some(LogSignal::Pass(label)), Some(LogSignal::Pass(_))) => return Ok(label.into()),
            (_, Some(LogSignal::Fatal(label))) => bail!("remote joining peer reported {label}"),
            _ => {}
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

fn launch_listener(config: &Config, port: u16, log_path: &Path, dry_run: bool) -> Result<Child> {
    let args = listener_args(config, port);
    spawn_logged(config, &args, log_path, dry_run, "Local listening peer")
}

fn wait_for_join_addr(
    config: &Config,
    log_path: &Path,
    preferred_ip: IpAddr,
    timeout: Duration,
) -> Result<String> {
    let started = Instant::now();
    loop {
        let text = read_lossy(log_path);
        if let Some(addr) =
            parse_join_addr_prefer_ip(&text, &config.webrtc.join_addr_marker, Some(preferred_ip))
        {
            return Ok(addr);
        }
        if let Some(LogSignal::Fatal(label)) = classify_log(&config.process, &text) {
            bail!("local listening peer reported {label} before WebRTC address was emitted");
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
}

struct AndroidOneRunOptions<'a> {
    port: u16,
    local_ip: IpAddr,
    timeout: Duration,
    dry_run: bool,
    launch_config: Option<&'a str>,
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

    let max_runs = options.max_runs.unwrap_or(config.webrtc.max_runs);
    let timeout = Duration::from_secs(options.timeout_secs.unwrap_or(config.webrtc.timeout_secs));
    let port = options.webrtc_port.unwrap_or(config.webrtc.port);
    let local_ip = options.local_ip.unwrap_or(config.webrtc.local_ip);

    if !options.no_build {
        run_build(&config, options.dry_run)?;
        run_apk_build(&config, &android_cfg, options.dry_run)?;
    }

    // Boot the emulator once for the whole run series — re-booting per run is
    // 30-60s of overhead. We still install / uninstall fresh state each run.
    let mut emulator = android::prepare_target(&android_cfg, options.dry_run)?;
    let install_result = if options.no_install {
        Ok(())
    } else {
        android::install_apk(&android_cfg, options.dry_run)
    };
    if let Err(err) = install_result {
        android::kill_emulator(&android_cfg, emulator.take());
        return Err(err);
    }

    let mut passed = 0;
    let mut failed = 0;
    for run in 1..=max_runs {
        println!("=== ANDROID RUN {run}/{max_runs} ===");
        let report = run_one_android(
            &config,
            &android_cfg,
            run,
            AndroidOneRunOptions {
                port,
                local_ip,
                timeout,
                dry_run: options.dry_run,
                launch_config: options.launch_config.as_deref(),
            },
        );
        let report = match report {
            Ok(r) => r,
            Err(e) => RunReport {
                run,
                status: RunStatus::Fail,
                label: e.to_string(),
                duration_secs: 0,
            },
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

    android::kill_emulator(&android_cfg, emulator);

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

    let max_runs = options.max_runs.unwrap_or(config.webrtc.max_runs);
    let timeout = Duration::from_secs(options.timeout_secs.unwrap_or(60));

    if !options.no_build {
        run_build(&config, options.dry_run)?;
        run_apk_build(&config, &android_cfg, options.dry_run)?;
    }

    let mut emulator = android::prepare_target(&android_cfg, options.dry_run)?;
    let install_result = if options.no_install {
        Ok(())
    } else {
        android::install_apk(&android_cfg, options.dry_run)
    };
    if let Err(err) = install_result {
        android::kill_emulator(&android_cfg, emulator.take());
        return Err(err);
    }

    let mut passed = 0;
    let mut failed = 0;
    for run in 1..=max_runs {
        println!("=== ANDROID UI RUN {run}/{max_runs} ===");
        let report = run_one_android_ui(
            &config,
            &android_cfg,
            run,
            timeout,
            options.dry_run,
            options.launch_config.as_deref(),
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

    android::kill_emulator(&android_cfg, emulator);

    println!("=== SUMMARY ===");
    println!("{passed}/{max_runs} passed, {failed} failed");
    if failed == 0 {
        Ok(())
    } else {
        bail!("{failed} android UI run(s) failed")
    }
}

pub fn run_android_doctor(options: AndroidDoctorOptions) -> Result<()> {
    let config = load_config(&options.config_path)?;
    let mut android_cfg = config
        .android
        .as_ref()
        .context("[android] section is required for android-doctor; see fragpipe README")?
        .clone();
    apply_android_overrides(&mut android_cfg, options.adb_serial, options.device);

    println!("=== ANDROID DOCTOR ===");
    let apk = resolve_path(project_root(&config), &android_cfg.apk_path);
    let ui_cfg = android_ui_config(android_cfg.clone());
    let ui_apk = resolve_path(project_root(&config), &ui_cfg.apk_path);
    println!("target: {:?}", android_cfg.target);
    println!("package: {}", android_cfg.package_name);
    println!("activity: {}", android_cfg.activity_name);
    println!("apk: {}", apk.display());
    if !apk.exists() {
        bail!("configured APK does not exist: {}", apk.display());
    }
    if ui_apk != apk {
        println!("ui package: {}", ui_cfg.package_name);
        println!("ui activity: {}", ui_cfg.activity_name);
        println!("ui apk: {}", ui_apk.display());
        if !ui_apk.exists() {
            bail!("configured UI APK does not exist: {}", ui_apk.display());
        }
    }
    android::adb_bin(&android_cfg)?;
    if android_cfg.target == AndroidTarget::Emulator {
        android::emulator_bin(&android_cfg)?;
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
        println!("avd: {}", android_cfg.avd_name);
    } else if android_cfg.adb_serial.is_none() {
        bail!("device target requires adb_serial or --adb-serial");
    }
    println!("android doctor passed");
    Ok(())
}

fn run_one_android(
    config: &Config,
    android_cfg: &crate::config::AndroidConfig,
    run: u32,
    options: AndroidOneRunOptions<'_>,
) -> Result<RunReport> {
    let started = Instant::now();
    // Stop any leftover instance from a previous run, then clear logs.
    if !options.dry_run {
        let _ = android::force_stop(android_cfg);
    }
    let listener_log = config.game.listener_log.clone();
    remove_if_exists(&listener_log)?;
    remove_if_exists(&android_cfg.logcat_log)?;

    let mut listener = launch_listener(config, options.port, &listener_log, options.dry_run)?;
    let mut logcat: Option<Child> = None;

    let result: Result<String> = (|| {
        let raw_addr = if options.dry_run {
            "/ip4/127.0.0.1/udp/27200/webrtc-direct/certhash/uEiDryRunCerthash".to_string()
        } else {
            wait_for_join_addr(config, &listener_log, options.local_ip, options.timeout)?
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
        config,
        android_cfg,
        "android-1v1",
        run,
        &report,
        &[
            (&listener_log, "desktop-listening-peer.log"),
            (&android_cfg.logcat_log, "android-logcat.log"),
        ],
    )?;
    Ok(report)
}

fn run_one_android_ui(
    config: &Config,
    android_cfg: &AndroidConfig,
    run: u32,
    timeout: Duration,
    dry_run: bool,
    launch_config: Option<&str>,
) -> Result<RunReport> {
    let started = Instant::now();
    if !dry_run {
        let _ = android::force_stop(android_cfg);
    }
    remove_if_exists(&android_cfg.logcat_log)?;
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
        loop {
            let logcat_text = read_lossy(&android_cfg.logcat_log);
            if let Some(label) = classify_non_success_log(&config.process, &logcat_text) {
                bail!("android UI app reported {label}");
            }
            if started.elapsed() >= Duration::from_secs(5) {
                android::assert_landscape(android_cfg, false)?;
                android::capture_screenshot(android_cfg, &screenshot_path, false)?;
                return Ok("LANDSCAPE_SCREENSHOT".into());
            }
            if started.elapsed() > timeout {
                bail!(
                    "timed out after {}s waiting for android UI screenshot",
                    timeout.as_secs()
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
        config,
        android_cfg,
        "android-ui",
        run,
        &report,
        &[
            (&android_cfg.logcat_log, "android-logcat.log"),
            (&screenshot_path, "screenshot.png"),
        ],
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

fn run_apk_build(config: &Config, android_cfg: &AndroidConfig, dry_run: bool) -> Result<()> {
    if let Some(command) = android_cfg.apk_build_command.as_deref() {
        run_shell_command("Android APK build", command, project_root(config), dry_run)?;
    }
    Ok(())
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
    cfg
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
    config: &Config,
    android_cfg: &AndroidConfig,
    mode: &str,
    run: u32,
    report: &RunReport,
    files: &[(&Path, &str)],
) -> Result<()> {
    let root = project_root(config)
        .join("logs")
        .join("fragpipe")
        .join(format!("{}_{}", timestamp_secs(), mode));
    let run_dir = root.join(format!("run-{run}"));
    fs::create_dir_all(&run_dir)
        .with_context(|| format!("failed to create artifact dir {}", run_dir.display()))?;
    for (src, name) in files {
        if src.exists() {
            let _ = fs::copy(src, run_dir.join(name));
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
    }
}
