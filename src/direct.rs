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

use crate::config::{
    Config, DirectConfig, RemotePeer, TournamentConfig, load_config, select_remote,
};
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

pub struct TournamentRunOptions {
    pub config_path: PathBuf,
    pub remote: String,
    pub max_runs: Option<u32>,
    pub timeout_secs: Option<u64>,
    pub stop_on_failure: bool,
    pub no_build: bool,
    pub no_deploy: bool,
    pub local_ip: Option<IpAddr>,
    pub dry_run: bool,
    pub output_format: OutputFormat,
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

pub fn run_direct_tournament(options: TournamentRunOptions) -> Result<()> {
    let config = load_config(&options.config_path)?;
    let tournament = config
        .tournament
        .as_ref()
        .context("direct-tournament requires a [tournament] config section")?;
    let remote = select_remote(&config, &options.remote)?;
    let max_runs = options.max_runs.unwrap_or(tournament.max_runs);
    let timeout = Duration::from_secs(options.timeout_secs.unwrap_or(tournament.timeout_secs));
    let local_ip = options.local_ip.unwrap_or(tournament.local_ip);
    let total_players = tournament
        .local_players
        .checked_add(tournament.remote_players)
        .context("direct-tournament player count overflow")?;
    if max_runs == 0 || timeout.is_zero() || tournament.port == 0 {
        bail!("direct-tournament runs, timeout, and port must be greater than zero");
    }
    if tournament.local_players == 0 || tournament.remote_players == 0 || total_players < 2 {
        bail!("direct-tournament requires local and remote players");
    }
    if remote.restricted {
        bail!("direct-tournament requires an unrestricted SSH peer");
    }

    println!("Fragpipe project: {}", config.game.name);
    println!(
        "Direct LAN tournament: {} local + {} on {} via {} ({}:{})",
        tournament.local_players,
        tournament.remote_players,
        remote.name,
        remote.host,
        local_ip,
        tournament.port,
    );

    if !options.no_build {
        run_build(&config, options.dry_run)?;
    }
    if !options.no_deploy {
        ssh::deploy(&config, remote, options.dry_run)?;
    }

    let context = TournamentRunContext {
        config: &config,
        tournament,
        remote,
        local_ip,
        timeout,
        dry_run: options.dry_run,
    };
    let mut passed = 0;
    let mut failed = 0;
    for run in 1..=max_runs {
        println!("=== RUN {run}/{max_runs} ===");
        let report = run_tournament_once(&context, run);
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
        bail!("{failed} direct LAN tournament run(s) failed")
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

struct TournamentRunContext<'a> {
    config: &'a Config,
    tournament: &'a TournamentConfig,
    remote: &'a RemotePeer,
    local_ip: IpAddr,
    timeout: Duration,
    dry_run: bool,
}

struct LocalTournamentPeer {
    index: u8,
    child: Child,
    log: PathBuf,
    passed: bool,
    exited: bool,
}

struct RemoteTournamentPeer {
    index: u8,
    passed: bool,
    exited: bool,
}

fn run_tournament_once(context: &TournamentRunContext<'_>, run: u32) -> RunReport {
    let started = Instant::now();
    let run_dir = crate::config::resolve_path(
        crate::config::project_root(context.config),
        &context.tournament.artifact_dir,
    )
    .join(format!("run-{run:02}"));
    let result = run_tournament_once_inner(context, &run_dir);
    let captured = capture_and_validate_tournament_logs(context, &run_dir);
    let result = match (result, captured) {
        (Ok(label), Ok(())) => Ok(label),
        (Ok(_), Err(error)) | (Err(error), _) => Err(error),
    };
    let duration_secs = started.elapsed().as_secs();
    let report = match result {
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
    };
    if !context.dry_run
        && let Err(error) = fs::create_dir_all(&run_dir).and_then(|()| {
            fs::write(
                run_dir.join("report.json"),
                serde_json::to_vec_pretty(&report).unwrap_or_default(),
            )
        })
    {
        eprintln!("warning: failed to write tournament report: {error}");
    }
    report
}

fn run_tournament_once_inner(context: &TournamentRunContext<'_>, run_dir: &Path) -> Result<String> {
    if !context.dry_run {
        fs::create_dir_all(run_dir)
            .with_context(|| format!("failed to create {}", run_dir.display()))?;
    }

    let first_remote = context.tournament.local_players;
    let total_players = first_remote + context.tournament.remote_players;
    for index in first_remote..total_players {
        ssh::stop_remote_instance(context.remote, index, context.dry_run)?;
        ssh::clear_remote_instance_log(context.remote, index, context.dry_run)?;
    }

    let host_log = run_dir.join("peer-0.log");
    let host_args = tournament_args(
        &context.tournament.host_args,
        context.config,
        context.local_ip,
        context.tournament.port,
        0,
        true,
    );
    let host = spawn_logged(
        context.config,
        &host_args,
        &host_log,
        context.dry_run,
        "Local tournament host",
    )?;
    let mut local = vec![LocalTournamentPeer {
        index: 0,
        child: host,
        log: host_log,
        passed: false,
        exited: false,
    }];
    let mut remote = Vec::new();

    let result = (|| {
        if !context.dry_run {
            let host_peer = &mut local[0];
            wait_for_process_marker(
                context.config,
                &mut host_peer.child,
                &host_peer.log,
                &context.tournament.ready_marker,
                context.timeout,
                "local tournament host",
            )?;
        }

        for index in 1..context.tournament.local_players {
            let log = run_dir.join(format!("peer-{index}.log"));
            let args = tournament_args(
                &context.tournament.joiner_args,
                context.config,
                context.local_ip,
                context.tournament.port,
                index,
                false,
            );
            local.push(LocalTournamentPeer {
                index,
                child: spawn_logged(
                    context.config,
                    &args,
                    &log,
                    context.dry_run,
                    &format!("Local tournament peer {index}"),
                )?,
                log,
                passed: false,
                exited: false,
            });
            stagger(context.tournament.stagger_ms, context.dry_run);
        }

        for index in first_remote..total_players {
            let args = tournament_args(
                &context.tournament.joiner_args,
                context.config,
                context.local_ip,
                context.tournament.port,
                index,
                false,
            );
            ssh::launch_remote_instance(
                context.config,
                context.remote,
                index,
                &args,
                context.dry_run,
            )?;
            remote.push(RemoteTournamentPeer {
                index,
                passed: false,
                exited: false,
            });
            stagger(context.tournament.stagger_ms, context.dry_run);
        }

        if context.dry_run {
            return Ok("DRY_RUN".into());
        }
        watch_tournament(context, &mut local, &mut remote)
    })();

    let mut cleanup_error = None;
    for peer in &mut local {
        kill_child(&mut peer.child);
    }
    for index in first_remote..total_players {
        if let Err(error) = ssh::stop_remote_instance(context.remote, index, context.dry_run) {
            cleanup_error.get_or_insert(error);
        }
    }
    if result.is_ok()
        && let Some(error) = cleanup_error
    {
        return Err(error);
    }
    result
}

fn watch_tournament(
    context: &TournamentRunContext<'_>,
    local: &mut [LocalTournamentPeer],
    remote: &mut [RemoteTournamentPeer],
) -> Result<String> {
    let started = Instant::now();
    loop {
        for peer in &mut *local {
            let text = read_lossy(&peer.log);
            if let Some(label) = classify_non_success_log(&context.config.process, &text) {
                bail!("local tournament peer {} reported {label}", peer.index);
            }
            peer.passed |= text.contains(&context.tournament.pass_marker);
            if !peer.exited
                && let Some(status) = peer.child.try_wait().with_context(|| {
                    format!("failed to poll local tournament peer {}", peer.index)
                })?
            {
                if !status.success() || !peer.passed {
                    bail!(
                        "local tournament peer {} exited before completion: {status}",
                        peer.index
                    );
                }
                peer.exited = true;
            }
        }

        // ponytail: one SSH poll per remote peer; batch by host if sessions grow past single digits.
        for peer in &mut *remote {
            let snapshot = ssh::remote_instance_snapshot(context.remote, peer.index)?;
            if let Some(label) = classify_non_success_log(&context.config.process, &snapshot.log) {
                bail!("remote tournament peer {} reported {label}", peer.index);
            }
            peer.passed |= snapshot.log.contains(&context.tournament.pass_marker);
            peer.exited = !snapshot.running;
            if peer.exited && (snapshot.exit_code != Some(0) || !peer.passed) {
                bail!(
                    "remote tournament peer {} exited before completion with status {:?}",
                    peer.index,
                    snapshot.exit_code
                );
            }
        }

        if local.iter().all(|peer| peer.passed && peer.exited)
            && remote.iter().all(|peer| peer.passed && peer.exited)
        {
            return Ok(context.tournament.pass_marker.clone());
        }
        if started.elapsed() > context.timeout {
            bail!(
                "timed out after {}s waiting for all tournament peers to complete",
                context.timeout.as_secs()
            );
        }
        thread::sleep(Duration::from_secs(1));
    }
}

fn capture_and_validate_tournament_logs(
    context: &TournamentRunContext<'_>,
    run_dir: &Path,
) -> Result<()> {
    if context.dry_run {
        return Ok(());
    }
    let first_remote = context.tournament.local_players;
    let total_players = first_remote + context.tournament.remote_players;
    for index in first_remote..total_players {
        let text = ssh::remote_instance_log(context.remote, index)?;
        fs::write(run_dir.join(format!("peer-{index}.log")), text)
            .with_context(|| format!("failed to preserve remote tournament peer {index} log"))?;
    }
    for index in 0..total_players {
        let text = read_lossy(&run_dir.join(format!("peer-{index}.log")));
        if let Some(label) = classify_non_success_log(&context.config.process, &text) {
            bail!("tournament peer {index} reported {label}");
        }
        if !text.contains(&context.tournament.pass_marker) {
            bail!("tournament peer {index} is missing the completion marker");
        }
    }
    Ok(())
}

fn stagger(milliseconds: u64, dry_run: bool) {
    if !dry_run && milliseconds > 0 {
        thread::sleep(Duration::from_millis(milliseconds));
    }
}

fn tournament_args(
    configured: &[String],
    config: &Config,
    local_ip: IpAddr,
    port: u16,
    index: u8,
    host: bool,
) -> Vec<String> {
    let mut args = configured.to_vec();
    for arg in &mut args {
        *arg = arg
            .replace("{local_ip}", &local_ip.to_string())
            .replace("{port}", &port.to_string())
            .replace("{index}", &index.to_string());
    }
    args.extend(if host {
        config.game.listener_extra_args.clone()
    } else {
        config.game.joiner_extra_args.clone()
    });
    args
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
    // Always reap the local process even if the remote cleanup command fails;
    // otherwise a transient SSH outage leaks a listener into the next run.
    let stop_result = ssh::stop_remote(config, remote, false);
    kill_child(&mut listener);
    stop_result?;
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
        // Read the logs before polling the child.  The game writes its pass
        // marker and then exits cleanly; polling first races that final write
        // and turns a successful game-over into a false failure.
        let listener_text = read_lossy(listener_log);
        let remote_text = ssh::remote_log(remote)?;
        if let Some(label) = classify_non_success_log(&config.process, &listener_text) {
            bail!("local LAN listening peer reported {label}");
        }
        if let Some(label) = classify_non_success_log(&config.process, &remote_text) {
            bail!("remote LAN joining peer reported {label}");
        }
        if let (Some(LogSignal::Pass(label)), Some(LogSignal::Pass(_))) = (
            classify_log(&config.process, &listener_text),
            classify_log(&config.process, &remote_text),
        ) {
            return Ok(label.into());
        }

        if let Some(status) = listener
            .try_wait()
            .context("failed to poll local LAN listening peer")?
        {
            bail!("local LAN listening peer exited before pass marker: {status}");
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
    wait_for_process_marker(
        config,
        listener,
        listener_log,
        &config.direct.ready_marker,
        timeout,
        "local LAN listener",
    )
}

fn wait_for_process_marker(
    config: &Config,
    process: &mut Child,
    log: &Path,
    marker: &str,
    timeout: Duration,
    label: &str,
) -> Result<()> {
    let started = Instant::now();
    while !read_lossy(log).contains(marker) {
        if let Some(status) = process
            .try_wait()
            .with_context(|| format!("failed to poll {label} while waiting for readiness"))?
        {
            bail!("{label} exited before readiness marker: {status}");
        }
        if let Some(fatal) = classify_non_success_log(&config.process, &read_lossy(log)) {
            bail!("{label} reported {fatal} before readiness");
        }
        if started.elapsed() > timeout {
            bail!(
                "timed out after {}s waiting for readiness marker `{}`",
                timeout.as_secs(),
                marker
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

    #[test]
    fn tournament_args_render_unique_peer_identity() {
        let config: Config = toml::from_str(
            r#"
            [game]
            binary = "game"
            "#,
        )
        .unwrap();
        assert_eq!(
            tournament_args(
                &[
                    "--join".into(),
                    "{local_ip}:{port}".into(),
                    "--identity-dir".into(),
                    "identity-{index}".into(),
                ],
                &config,
                "10.10.0.1".parse().unwrap(),
                27100,
                4,
                false,
            ),
            vec!["--join", "10.10.0.1:27100", "--identity-dir", "identity-4"]
        );
    }
}
