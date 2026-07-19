//! Physical LAN 1v1 orchestration.
//!
//! The listener runs locally and the joining peer is deployed/launched over
//! SSH.  The game transport remains the game's responsibility; Fragpipe only
//! supplies the UDP host/join arguments and watches both logs for the same
//! pass/fatal markers used by the other runners.

use std::fs;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::config::{Config, DirectConfig, RemotePeer, load_config, select_remote};
use crate::logwatch::{LogSignal, classify_log, classify_non_success_log, read_lossy};
use crate::process::{kill_child, remove_if_exists, run_build, spawn_logged};
use crate::ssh;

pub struct DirectRunOptions {
    pub config_path: PathBuf,
    pub remote: String,
    pub max_runs: Option<u32>,
    pub timeout_secs: Option<u64>,
    pub stop_on_failure: bool,
    pub no_build: bool,
    pub no_deploy: bool,
    pub local_ip: Option<IpAddr>,
    pub port: Option<u16>,
    pub headless: bool,
    pub dry_run: bool,
    pub output_format: OutputFormat,
    pub transport: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
enum RunStatus {
    Pass,
    Fail,
    Timeout,
}

#[derive(Debug, Serialize)]
struct RunReport {
    run: u32,
    status: RunStatus,
    label: String,
    duration_secs: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    Text,
    Jsonl,
}

pub fn run_direct_1v1(options: DirectRunOptions) -> Result<()> {
    if let Some(transport) = options.transport.as_deref()
        && transport != "lan"
    {
        bail!("direct-1v1 currently supports only transport=lan; got {transport:?}");
    }

    let config = load_config(&options.config_path)?;
    let remote = select_remote(&config, &options.remote)?;
    let max_runs = options.max_runs.unwrap_or(config.direct.max_runs);
    let timeout = Duration::from_secs(options.timeout_secs.unwrap_or(config.direct.timeout_secs));
    let local_ip = options.local_ip.unwrap_or(config.direct.local_ip);
    let port = options.port.unwrap_or(config.direct.port);
    if max_runs == 0 {
        bail!("direct-1v1 requires at least one run");
    }
    if timeout.is_zero() {
        bail!("direct-1v1 timeout must be greater than zero");
    }
    if port == 0 {
        bail!("direct-1v1 UDP port must be greater than zero");
    }

    println!("Fragpipe project: {}", config.game.name);
    println!(
        "Direct LAN peer: {} via {} ({}:{})",
        remote.name, remote.host, local_ip, port
    );

    if !options.no_build {
        run_build(&config, options.dry_run)?;
    }
    if !options.no_deploy {
        ssh::deploy(&config, remote, options.dry_run)?;
    }

    let mut passed = 0;
    let mut failed = 0;
    for run in 1..=max_runs {
        println!("=== RUN {run}/{max_runs} ===");
        let context = DirectRunContext {
            config: &config,
            remote,
            local_ip,
            port,
            timeout,
            headless: options.headless,
            dry_run: options.dry_run,
        };
        let report = run_one(&context, run);
        write_artifacts(&context, run, &report);
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
        bail!("{failed} direct LAN 1v1 run(s) failed")
    }
}

struct DirectRunContext<'a> {
    config: &'a Config,
    remote: &'a RemotePeer,
    local_ip: IpAddr,
    port: u16,
    timeout: Duration,
    headless: bool,
    dry_run: bool,
}

fn run_one(context: &DirectRunContext<'_>, run: u32) -> RunReport {
    let started = Instant::now();
    let result = run_one_inner(context);
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

fn run_one_inner(context: &DirectRunContext<'_>) -> Result<String> {
    let config = context.config;
    let remote = context.remote;
    let listener_log = config.game.listener_log.clone();
    remove_if_exists(&listener_log)?;
    ssh::stop_remote(config, remote, context.dry_run)?;
    ssh::clear_remote_log(remote, context.dry_run)?;

    let listener_args = listener_args(&config.direct, config, context.port, context.headless);
    let mut listener = spawn_logged(
        config,
        &listener_args,
        &listener_log,
        context.dry_run,
        "Local LAN listening peer",
    )?;

    if context.dry_run {
        let args = joiner_args(
            &config.direct,
            config,
            context.local_ip,
            context.port,
            context.headless,
        );
        ssh::launch_remote(config, remote, &args, true)?;
        kill_child(&mut listener);
        return Ok("DRY_RUN".into());
    }

    let result = run_one_live(context, &mut listener, &listener_log);
    ssh::stop_remote(config, remote, false)?;
    kill_child(&mut listener);
    result
}

fn run_one_live(
    context: &DirectRunContext<'_>,
    listener: &mut Child,
    listener_log: &Path,
) -> Result<String> {
    let config = context.config;
    let remote = context.remote;
    let timeout = context.timeout;
    wait_for_ready(config, listener, listener_log, timeout)?;
    let args = joiner_args(
        &config.direct,
        config,
        context.local_ip,
        context.port,
        context.headless,
    );
    ssh::launch_remote(config, remote, &args, false)?;

    let started = Instant::now();
    loop {
        if let Some(status) = listener
            .try_wait()
            .context("failed to poll local LAN listening peer")?
        {
            bail!("local LAN listening peer exited before pass marker: {status}");
        }

        let listener_text = read_lossy(listener_log);
        if let Some(label) = classify_non_success_log(&config.process, &listener_text) {
            bail!("local LAN listening peer reported {label}");
        }

        let remote_text = ssh::remote_log(remote)?;
        if let Some(label) = classify_non_success_log(&config.process, &remote_text) {
            bail!("remote LAN joining peer reported {label}");
        }
        if let (Some(LogSignal::Pass(label)), Some(LogSignal::Pass(_))) = (
            classify_log(&config.process, &listener_text),
            classify_log(&config.process, &remote_text),
        ) {
            return Ok(label.into());
        }

        if started.elapsed() > timeout {
            bail!(
                "timed out after {}s waiting for both LAN peers to reach a pass marker",
                timeout.as_secs()
            );
        }
        thread::sleep(Duration::from_secs(1));
    }
}

fn wait_for_ready(
    config: &Config,
    listener: &mut Child,
    listener_log: &Path,
    timeout: Duration,
) -> Result<()> {
    let started = Instant::now();
    while !read_lossy(listener_log).contains(&config.direct.ready_marker) {
        if let Some(status) = listener
            .try_wait()
            .context("failed to poll local LAN listener while waiting for readiness")?
        {
            bail!("local LAN listener exited before readiness marker: {status}");
        }
        if let Some(label) = classify_non_success_log(&config.process, &read_lossy(listener_log)) {
            bail!("local LAN listener reported {label} before readiness");
        }
        if started.elapsed() > timeout {
            bail!(
                "timed out after {}s waiting for LAN readiness marker `{}`",
                timeout.as_secs(),
                config.direct.ready_marker
            );
        }
        thread::sleep(Duration::from_millis(250));
    }
    Ok(())
}

fn listener_args(direct: &DirectConfig, config: &Config, port: u16, headless: bool) -> Vec<String> {
    let mut args = if direct.listener_args.is_empty() {
        vec![
            "--auto-host-udp".into(),
            "--udp-addr".into(),
            "{port}".into(),
            "--auto-play".into(),
        ]
    } else {
        direct.listener_args.clone()
    };
    render_placeholders(&mut args, port, None);
    args.extend(config.game.listener_extra_args.clone());
    if headless && !args.iter().any(|arg| arg == "--headless") {
        args.push("--headless".into());
    }
    args
}

fn joiner_args(
    direct: &DirectConfig,
    config: &Config,
    local_ip: IpAddr,
    port: u16,
    headless: bool,
) -> Vec<String> {
    let mut args = if direct.joiner_args.is_empty() {
        vec![
            "--auto-join-udp".into(),
            "--udp-addr".into(),
            "{local_ip}:{port}".into(),
            "--auto-play".into(),
        ]
    } else {
        direct.joiner_args.clone()
    };
    render_placeholders(&mut args, port, Some(local_ip));
    args.extend(config.game.joiner_extra_args.clone());
    if headless && !args.iter().any(|arg| arg == "--headless") {
        args.push("--headless".into());
    }
    args
}

fn render_placeholders(args: &mut [String], port: u16, local_ip: Option<IpAddr>) {
    for arg in args {
        *arg = arg.replace("{port}", &port.to_string());
        if let Some(local_ip) = local_ip {
            *arg = arg.replace("{local_ip}", &local_ip.to_string());
        }
    }
}

fn emit_report(format: OutputFormat, report: &RunReport) -> Result<()> {
    match format {
        OutputFormat::Text => {
            println!(
                "--- {:?} ({}, {}s) ---",
                report.status, report.label, report.duration_secs
            );
        }
        OutputFormat::Jsonl => println!("{}", serde_json::to_string(report)?),
    }
    Ok(())
}

/// Preserve enough evidence to diagnose a failed run after the next run has
/// cleared the live logs. Artifact writes are best-effort: the test result is
/// still authoritative when a read-only artifact directory is unavailable.
fn write_artifacts(context: &DirectRunContext<'_>, run: u32, report: &RunReport) {
    if context.dry_run {
        return;
    }
    let root = crate::config::project_root(context.config);
    let dir = crate::config::resolve_path(root, &context.config.direct.artifact_dir)
        .join(format!("run-{run:02}"));
    if let Err(error) = fs::create_dir_all(&dir) {
        eprintln!(
            "warning: failed to create direct-run artifact directory {}: {error}",
            dir.display()
        );
        return;
    }
    let write = |name: &str, contents: &[u8]| {
        if let Err(error) = fs::write(dir.join(name), contents) {
            eprintln!("warning: failed to write direct-run artifact {name}: {error}");
        }
    };
    let listener_log = crate::config::resolve_path(root, &context.config.game.listener_log);
    if let Ok(contents) = fs::read(&listener_log) {
        write("listener.log", &contents);
    }
    if let Ok(remote_log) = ssh::remote_log(context.remote) {
        write("remote.log", remote_log.as_bytes());
    }
    if let Ok(summary) = serde_json::to_vec_pretty(report) {
        write("report.json", &summary);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direct_listener_args_render_port_and_headless() {
        let config: Config = toml::from_str(
            r#"
            [game]
            binary = "game"
            [direct]
            listener_args = ["--auto-host-udp", "--udp-addr", "{port}"]
            "#,
        )
        .unwrap();
        assert_eq!(
            listener_args(&config.direct, &config, 27100, true),
            vec!["--auto-host-udp", "--udp-addr", "27100", "--headless"]
        );
    }

    #[test]
    fn direct_joiner_args_render_ip_and_port() {
        let config: Config = toml::from_str(
            r#"
            [game]
            binary = "game"
            "#,
        )
        .unwrap();
        assert_eq!(
            joiner_args(
                &config.direct,
                &config,
                "10.10.0.1".parse().unwrap(),
                27100,
                false,
            ),
            vec![
                "--auto-join-udp",
                "--udp-addr",
                "10.10.0.1:27100",
                "--auto-play"
            ]
        );
    }
}
