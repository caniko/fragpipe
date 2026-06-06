use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::time::{Duration, Instant};

use rmcp::{
    ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::*,
    tool, tool_handler, tool_router,
    transport::stdio,
};

#[derive(Clone)]
struct FragpipeMcp {
    default_workdir: PathBuf,
    #[allow(dead_code)]
    tool_router: ToolRouter<Self>,
}

impl FragpipeMcp {
    fn new() -> Self {
        Self {
            default_workdir: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            tool_router: Self::tool_router(),
        }
    }

    fn command_context(&self, common: &CommonInput) -> CommandContext {
        CommandContext {
            fragpipe_bin: common
                .fragpipe_bin
                .clone()
                .unwrap_or_else(|| "fragpipe".to_string()),
            workdir: common
                .workdir
                .clone()
                .map(PathBuf::from)
                .unwrap_or_else(|| self.default_workdir.clone()),
        }
    }

    fn run_fragpipe(
        &self,
        common: &CommonInput,
        args: Vec<String>,
        timeout_secs: u64,
    ) -> Result<(String, bool), ErrorData> {
        let ctx = self.command_context(common);
        let mut cmd = Command::new(&ctx.fragpipe_bin);
        cmd.args(&args)
            .current_dir(&ctx.workdir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt as _;
            cmd.process_group(0);
        }

        let child = cmd.spawn().map_err(|error| {
            ErrorData::internal_error(
                format!(
                    "failed to spawn fragpipe binary `{}` in {}: {error}",
                    ctx.fragpipe_bin,
                    ctx.workdir.display()
                ),
                None,
            )
        })?;
        let mut child = ManagedChild::new(child);

        let started = Instant::now();
        loop {
            if let Some(status) = child.try_wait().map_err(|error| {
                ErrorData::internal_error(format!("failed to poll fragpipe process: {error}"), None)
            })? {
                let output = child.wait_with_output().map_err(|error| {
                    ErrorData::internal_error(
                        format!("failed to collect fragpipe output: {error}"),
                        None,
                    )
                })?;
                let mut text = command_header(&ctx, &args);
                text.push_str(&String::from_utf8_lossy(&output.stdout));
                text.push_str(&String::from_utf8_lossy(&output.stderr));
                let ok = status.success();
                text.push_str(if ok {
                    "\n--- PASS ---\n"
                } else {
                    "\n--- FAIL ---\n"
                });
                return Ok((text, ok));
            }

            if started.elapsed() >= Duration::from_secs(timeout_secs) {
                child.terminate();
                let output = child.wait_with_output().map_err(|error| {
                    ErrorData::internal_error(
                        format!("failed to collect timed-out fragpipe output: {error}"),
                        None,
                    )
                })?;
                let mut text = command_header(&ctx, &args);
                text.push_str(&String::from_utf8_lossy(&output.stdout));
                text.push_str(&String::from_utf8_lossy(&output.stderr));
                text.push_str(&format!("\n--- TIMEOUT ({timeout_secs}s limit) ---\n"));
                return Ok((text, false));
            }

            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn run_single(
        &self,
        command: &str,
        input: RunInput,
        default_timeout: u64,
    ) -> Result<CallToolResult, ErrorData> {
        let max_runs = input.common.max_runs.unwrap_or(1).min(50);
        let timeout = input.common.timeout.unwrap_or(default_timeout);
        let mut args = vec![command.to_string()];
        push_common_fragpipe_args(&mut args, &input.common, max_runs, timeout);
        if input.no_deploy.unwrap_or(false) {
            args.push("--no-deploy".into());
        }
        if let Some(remote) = input.remote {
            args.push("--remote".into());
            args.push(remote);
        }
        if let Some(local_ip) = input.local_ip {
            args.push("--local-ip".into());
            args.push(local_ip);
        }
        if let Some(port) = input.webrtc_port {
            args.push("--webrtc-port".into());
            args.push(port.to_string());
        }
        let (output, _) = self.run_fragpipe(
            &input.common,
            args,
            timeout.saturating_mul(max_runs as u64).saturating_add(120),
        )?;
        Ok(CallToolResult::success(vec![Content::text(output)]))
    }

    fn cluster_context(&self, common: &ClusterCommonInput) -> ClusterContext {
        let cluster = common.cluster.clone().unwrap_or_else(|| "1v1".to_string());
        let vm_count = common
            .vm_count
            .unwrap_or_else(|| default_vm_count(&cluster));
        ClusterContext {
            cluster_ctl_bin: common
                .cluster_ctl_bin
                .clone()
                .unwrap_or_else(|| "cluster-ctl".to_string()),
            workdir: common
                .workdir
                .clone()
                .map(PathBuf::from)
                .unwrap_or_else(|| self.default_workdir.clone()),
            cluster,
            vm_count,
        }
    }

    fn run_cluster_ctl(
        &self,
        common: &ClusterCommonInput,
        subcommand_args: Vec<String>,
        timeout_secs: u64,
    ) -> Result<(String, bool), ErrorData> {
        let ctx = self.cluster_context(common);
        let mut full_args = Vec::with_capacity(subcommand_args.len() + 4);
        full_args.push("--cluster".into());
        full_args.push(ctx.cluster.clone());
        full_args.push("--vm-count".into());
        full_args.push(ctx.vm_count.to_string());
        full_args.extend(subcommand_args.iter().cloned());

        let mut cmd = Command::new(&ctx.cluster_ctl_bin);
        cmd.args(&full_args)
            .current_dir(&ctx.workdir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt as _;
            cmd.process_group(0);
        }

        let child = cmd.spawn().map_err(|error| {
            ErrorData::internal_error(
                format!(
                    "failed to spawn cluster-ctl binary `{}` in {}: {error}",
                    ctx.cluster_ctl_bin,
                    ctx.workdir.display()
                ),
                None,
            )
        })?;
        let mut child = ManagedChild::new(child);

        let started = Instant::now();
        loop {
            if let Some(status) = child.try_wait().map_err(|error| {
                ErrorData::internal_error(format!("failed to poll cluster-ctl: {error}"), None)
            })? {
                let output = child.wait_with_output().map_err(|error| {
                    ErrorData::internal_error(
                        format!("failed to collect cluster-ctl output: {error}"),
                        None,
                    )
                })?;
                let mut text = cluster_command_header(&ctx, &subcommand_args);
                text.push_str(&String::from_utf8_lossy(&output.stdout));
                text.push_str(&String::from_utf8_lossy(&output.stderr));
                let ok = status.success();
                text.push_str(if ok {
                    "\n--- PASS ---\n"
                } else {
                    "\n--- FAIL ---\n"
                });
                return Ok((text, ok));
            }

            if started.elapsed() >= Duration::from_secs(timeout_secs) {
                child.terminate();
                let output = child.wait_with_output().map_err(|error| {
                    ErrorData::internal_error(
                        format!("failed to collect timed-out cluster-ctl output: {error}"),
                        None,
                    )
                })?;
                let mut text = cluster_command_header(&ctx, &subcommand_args);
                text.push_str(&String::from_utf8_lossy(&output.stdout));
                text.push_str(&String::from_utf8_lossy(&output.stderr));
                text.push_str(&format!("\n--- TIMEOUT ({timeout_secs}s limit) ---\n"));
                return Ok((text, false));
            }

            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn run_android(
        &self,
        command: &str,
        input: AndroidRunInput,
        default_timeout: u64,
    ) -> Result<(String, bool), ErrorData> {
        let max_runs = input.common.max_runs.unwrap_or(1).min(50);
        let timeout = input.common.timeout.unwrap_or(default_timeout);
        let mut args = vec![command.to_string()];
        push_common_fragpipe_args(&mut args, &input.common, max_runs, timeout);
        if input.no_install.unwrap_or(false) {
            args.push("--no-install".into());
        }
        if input.device.unwrap_or(false) {
            args.push("--device".into());
        }
        if let Some(serial) = input.adb_serial {
            args.push("--adb-serial".into());
            args.push(serial);
        }
        if let Some(config) = input.launch_config {
            args.push("--launch-config".into());
            args.push(config);
        }
        self.run_fragpipe(
            &input.common,
            args,
            timeout.saturating_mul(max_runs as u64).saturating_add(120),
        )
    }

    fn run_internet(
        &self,
        input: InternetRunInput,
        default_timeout: u64,
    ) -> Result<CallToolResult, ErrorData> {
        let max_runs = input.common.max_runs.unwrap_or(1).min(50);
        let timeout = input.common.timeout.unwrap_or(default_timeout);
        let mut args = vec!["internet-1v1".to_string()];
        if let Some(config) = &input.common.config {
            args.push("--config".into());
            args.push(config.clone());
        }
        args.push("--max-runs".into());
        args.push(max_runs.to_string());
        args.push("--timeout-secs".into());
        args.push(timeout.to_string());
        if input.common.stop_on_failure == Some(false) {
            args.push("--stop-on-failure=false".into());
        }
        let smoke_workdir = input
            .smoke_workdir
            .as_ref()
            .or(input.common.workdir.as_ref());
        if let Some(workdir) = smoke_workdir {
            args.push("--workdir".into());
            args.push(workdir.clone());
        }
        if let Some(game_bin) = input.game_bin {
            args.push("--game-bin".into());
            args.push(game_bin);
        }
        if let Some(rdv_bin) = input.rdv_bin {
            args.push("--rdv-bin".into());
            args.push(rdv_bin);
        }
        if let Some(asset_root) = input.asset_root {
            args.push("--asset-root".into());
            args.push(asset_root);
        }
        if let Some(log_dir) = input.log_dir {
            args.push("--log-dir".into());
            args.push(log_dir);
        }
        if let Some(pass_marker) = input.pass_marker {
            args.push("--pass-marker".into());
            args.push(pass_marker);
        }
        if input.common.dry_run.unwrap_or(false) {
            args.push("--dry-run".into());
        }
        let (output, _) = self.run_fragpipe(
            &input.common,
            args,
            timeout.saturating_mul(max_runs as u64).saturating_add(120),
        )?;
        Ok(CallToolResult::success(vec![Content::text(output)]))
    }
}

#[derive(Debug, Clone, serde::Deserialize, schemars::JsonSchema, Default)]
struct CommonInput {
    /// Fragpipe binary to execute. Defaults to `fragpipe` from PATH.
    #[serde(default)]
    fragpipe_bin: Option<String>,
    /// Working directory containing fragpipe config and project artifacts. Defaults to server cwd.
    #[serde(default)]
    workdir: Option<String>,
    /// Fragpipe config path. Required unless fragpipe's default `fragpipe.toml` is appropriate.
    #[serde(default)]
    config: Option<String>,
    /// Number of runs. Capped at 50.
    #[serde(default)]
    max_runs: Option<u32>,
    /// Per-run timeout in seconds.
    #[serde(default)]
    timeout: Option<u64>,
    /// If false, passes `--stop-on-failure=false`.
    #[serde(default)]
    stop_on_failure: Option<bool>,
    /// If true, passes `--no-build`.
    #[serde(default)]
    no_build: Option<bool>,
    /// If true, passes `--dry-run`.
    #[serde(default)]
    dry_run: Option<bool>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct RunInput {
    #[serde(flatten)]
    common: CommonInput,
    /// Skip SSH deploy for remote runs.
    #[serde(default)]
    no_deploy: Option<bool>,
    /// Configured remote name for SSH validation.
    #[serde(default)]
    remote: Option<String>,
    /// Local IP used to rewrite WebRTC listen addresses.
    #[serde(default)]
    local_ip: Option<String>,
    /// WebRTC listen port override.
    #[serde(default)]
    webrtc_port: Option<u16>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct InternetRunInput {
    #[serde(flatten)]
    common: CommonInput,
    /// Chessbender repo root passed to `fragpipe internet-1v1 --workdir`.
    /// Defaults to `workdir` when omitted.
    #[serde(default)]
    smoke_workdir: Option<String>,
    /// Chessbender binary passed as GAME_BIN.
    #[serde(default)]
    game_bin: Option<String>,
    /// thespan-rendezvous binary passed as RDV_BIN.
    #[serde(default)]
    rdv_bin: Option<String>,
    /// Asset root passed as ASSET_ROOT.
    #[serde(default)]
    asset_root: Option<String>,
    /// Log directory passed as LOG_DIR.
    #[serde(default)]
    log_dir: Option<String>,
    /// Pass marker passed as PASS_MARKER.
    #[serde(default)]
    pass_marker: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct AndroidRunInput {
    #[serde(flatten)]
    common: CommonInput,
    /// If true, passes `--no-install`.
    #[serde(default)]
    no_install: Option<bool>,
    /// If true, targets an attached physical device instead of booting the configured AVD.
    #[serde(default)]
    device: Option<bool>,
    /// adb serial for a physical device or specific emulator.
    #[serde(default)]
    adb_serial: Option<String>,
    /// JSON launch config pushed to the Android target before activity start.
    #[serde(default)]
    launch_config: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct AndroidDoctorInput {
    #[serde(flatten)]
    common: CommonInput,
    /// If true, targets an attached physical device instead of requiring the configured AVD.
    #[serde(default)]
    device: Option<bool>,
    /// adb serial for a physical device or specific emulator.
    #[serde(default)]
    adb_serial: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct CrossPlatformInput {
    #[serde(flatten)]
    common: CommonInput,
    /// Native WebRTC config path. Defaults to `config` or fragpipe default.
    #[serde(default)]
    native_config: Option<String>,
    /// Android config path. Defaults to `config` or fragpipe default.
    #[serde(default)]
    android_config: Option<String>,
    /// Enable native WebRTC cell. Defaults true.
    #[serde(default)]
    native_webrtc: Option<bool>,
    /// Enable Android WebRTC cell. Defaults true.
    #[serde(default)]
    android_1v1: Option<bool>,
    /// Enable Android UI cell. Defaults true.
    #[serde(default)]
    android_ui: Option<bool>,
    /// If true, targets an attached physical device for Android cells.
    #[serde(default)]
    device: Option<bool>,
    /// adb serial for Android cells.
    #[serde(default)]
    adb_serial: Option<String>,
    /// If true, passes `--no-install` to Android cells.
    #[serde(default)]
    no_install: Option<bool>,
    /// Cluster cells to run after the fragpipe cells (each shells out to
    /// `cluster-ctl test`). Steampipe expects VMs to be running, so each cell
    /// can optionally pre-run a flake app via `nix run .#<nix_up_target>` to
    /// boot its VMs. Cluster cells inherit `common.workdir`.
    #[serde(default)]
    cluster_cells: Option<Vec<ClusterCell>>,
    /// Override for the `cluster-ctl` binary used by cluster cells.
    #[serde(default)]
    cluster_ctl_bin: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize, schemars::JsonSchema)]
struct ClusterCell {
    /// Cluster name passed via `--cluster <name>`.
    cluster: String,
    /// Optional flake app to `nix run .#<target>` before the cluster_test invocation.
    #[serde(default)]
    nix_up_target: Option<String>,
    /// Override the flake reference for `nix_up_target` (default ".").
    #[serde(default)]
    nix_flake: Option<String>,
    /// VM count. Derived from cluster name when omitted (1v1 → 1, else → 7).
    #[serde(default)]
    vm_count: Option<u8>,
    /// Network transport: "lan" (default) or "steam".
    #[serde(default)]
    network: Option<String>,
    /// Players, 2-8.
    #[serde(default)]
    players: Option<u8>,
    /// Per-run no-progress timeout in seconds.
    #[serde(default)]
    timeout: Option<u64>,
    /// VM display mode (headless, weston, sway, weston-gpu, sway-gpu).
    #[serde(default)]
    display: Option<String>,
    /// Host game-binary args.
    #[serde(default)]
    host_args: Option<String>,
    /// VM game-binary args.
    #[serde(default)]
    vm_args: Option<String>,
    /// Capture screenshots on failure.
    #[serde(default)]
    capture_on_failure: Option<bool>,
    /// Display label for the matrix output (defaults to the cluster name).
    #[serde(default)]
    label: Option<String>,
}

struct CommandContext {
    fragpipe_bin: String,
    workdir: PathBuf,
}

struct ManagedChild {
    child: Option<Child>,
    pgid: u32,
}

impl ManagedChild {
    fn new(child: Child) -> Self {
        let pgid = child.id();
        Self {
            child: Some(child),
            pgid,
        }
    }

    fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
        self.child
            .as_mut()
            .expect("managed child missing before wait")
            .try_wait()
    }

    fn wait_with_output(mut self) -> std::io::Result<Output> {
        self.child
            .take()
            .expect("managed child missing before output collection")
            .wait_with_output()
    }

    fn terminate(&mut self) {
        terminate_process_group(self.pgid);
    }
}

impl Drop for ManagedChild {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take()
            && child.try_wait().ok().flatten().is_none()
        {
            #[cfg(not(unix))]
            let _ = child.kill();
            self.terminate();
            let _ = child.wait();
        }
    }
}

/// Shared inputs accepted by every `cluster_*` proxy tool.
///
/// Fragpipe-mcp shells out to the configured `cluster-ctl` binary so the same
/// MCP server can drive both fragpipe (bare-metal/Android) and steampipe
/// (cluster) operations. The defaults match steampipe's own MCP defaults so
/// existing skills can be re-pointed at fragpipe without behavioral drift.
#[derive(Debug, Clone, serde::Deserialize, schemars::JsonSchema, Default)]
struct ClusterCommonInput {
    /// `cluster-ctl` binary to execute. Defaults to `cluster-ctl` from PATH.
    #[serde(default)]
    cluster_ctl_bin: Option<String>,
    /// Working directory containing `steampipe.toml`. Defaults to server cwd.
    #[serde(default)]
    workdir: Option<String>,
    /// Cluster name: "1v1", "tournament", or a project-specific flavor (default: "1v1").
    #[serde(default)]
    cluster: Option<String>,
    /// Number of VMs. Derived from cluster name when omitted (1v1 → 1, else → 7).
    #[serde(default)]
    vm_count: Option<u8>,
    /// MCP-level wall-clock cap for the spawned `cluster-ctl` process, in seconds.
    /// Tool-specific defaults apply when omitted. For `cluster_test` this is
    /// derived from the per-run `timeout * max_runs` budget unless explicitly set.
    #[serde(default)]
    mcp_timeout: Option<u64>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct ClusterStatusInput {
    #[serde(flatten)]
    common: ClusterCommonInput,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct ClusterLogsInput {
    #[serde(flatten)]
    common: ClusterCommonInput,
    /// Specific VM target (e.g. "vm-1"). Omit for all VMs.
    #[serde(default)]
    vm: Option<String>,
    /// Max lines per VM (default: 100).
    #[serde(default)]
    max_lines: Option<u32>,
    /// Read from end (default true). Set to false for head mode.
    #[serde(default)]
    tail: Option<bool>,
    /// Regex pattern to filter log lines.
    #[serde(default)]
    pattern: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct ClusterDeployInput {
    #[serde(flatten)]
    common: ClusterCommonInput,
    /// Build the binary before deploying (default: true).
    #[serde(default)]
    build: Option<bool>,
    /// Verify deployment checksums after deploy (default: false).
    #[serde(default)]
    verify: Option<bool>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct ClusterStopInput {
    #[serde(flatten)]
    common: ClusterCommonInput,
    /// Also kill Steam and Weston processes (default: false).
    #[serde(default)]
    kill_steam: Option<bool>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct ClusterTestInput {
    #[serde(flatten)]
    common: ClusterCommonInput,
    /// Network transport: "lan" (default) or "steam".
    #[serde(default)]
    network: Option<String>,
    /// Number of players, 2-8 (default: derived from cluster).
    #[serde(default)]
    players: Option<u8>,
    /// Number of test runs (default: 1, max: 50).
    #[serde(default)]
    max_runs: Option<u32>,
    /// No-progress timeout per run in seconds (default: 300). Matches `cluster-ctl test --timeout`.
    #[serde(default)]
    timeout: Option<u64>,
    /// Emergency wall-clock timeout per run in seconds.
    #[serde(default)]
    hard_timeout: Option<u64>,
    /// Grace period (seconds) between SIGTERM and SIGKILL.
    #[serde(default)]
    shutdown_timeout: Option<u64>,
    /// Stop after first failure (default: true).
    #[serde(default)]
    stop_on_failure: Option<bool>,
    /// Deploy before testing (default: true).
    #[serde(default)]
    deploy: Option<bool>,
    /// Build before deploying (default: true).
    #[serde(default)]
    build: Option<bool>,
    /// VM display mode: "headless", "weston" (default), "sway", "weston-gpu", or "sway-gpu".
    #[serde(default)]
    display: Option<String>,
    /// Capture screenshots on failure (default: false).
    #[serde(default)]
    capture_on_failure: Option<bool>,
    /// Regex filter for output lines.
    #[serde(default)]
    filter_pattern: Option<String>,
    /// Write full output to this file (relative to workdir).
    #[serde(default)]
    output_file: Option<String>,
    /// Extra args for the host game binary.
    #[serde(default)]
    host_args: Option<String>,
    /// Extra args for VM game binaries.
    #[serde(default)]
    vm_args: Option<String>,
    /// Launch the local host process under strace.
    #[serde(default)]
    strace: Option<bool>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct ClusterHistoryInput {
    #[serde(flatten)]
    common: ClusterCommonInput,
    /// Show only the last N sessions.
    #[serde(default)]
    last: Option<usize>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct NixRunInput {
    /// Flake output target (e.g. "cluster-1v1-up", "cluster-tournament-test").
    target: String,
    /// Extra arguments passed after `--` to the flake app.
    #[serde(default)]
    args: Option<Vec<String>>,
    /// Flake reference (default: ".").
    #[serde(default)]
    flake: Option<String>,
    /// Working directory. Defaults to server cwd.
    #[serde(default)]
    workdir: Option<String>,
    /// Timeout in seconds (default: 300).
    #[serde(default)]
    timeout: Option<u64>,
}

struct ClusterContext {
    cluster_ctl_bin: String,
    workdir: PathBuf,
    cluster: String,
    vm_count: u8,
}

fn default_vm_count(cluster: &str) -> u8 {
    if cluster == "1v1" { 1 } else { 7 }
}

#[derive(Debug, Clone, Copy)]
struct ClusterTestArgOptions<'a> {
    network: Option<&'a str>,
    players: Option<u8>,
    max_runs: u32,
    timeout: u64,
    hard_timeout: Option<u64>,
    shutdown_timeout: Option<u64>,
    stop_on_failure: Option<bool>,
    deploy: Option<bool>,
    build: Option<bool>,
    display: Option<&'a str>,
    capture_on_failure: bool,
    filter_pattern: Option<&'a str>,
    output_file: Option<&'a str>,
    host_args: Option<&'a str>,
    vm_args: Option<&'a str>,
    strace: bool,
}

fn push_optional_value_arg(args: &mut Vec<String>, flag: &str, value: Option<&str>) {
    if let Some(value) = value {
        args.push(flag.into());
        args.push(value.into());
    }
}

fn push_optional_equals_arg(args: &mut Vec<String>, flag: &str, value: Option<&str>) {
    if let Some(value) = value {
        args.push(format!("{flag}={value}"));
    }
}

fn build_cluster_test_args(options: ClusterTestArgOptions<'_>) -> Vec<String> {
    let mut args: Vec<String> = vec!["test".into()];
    push_optional_value_arg(&mut args, "--network", options.network);
    if let Some(players) = options.players {
        args.push("--players".into());
        args.push(players.to_string());
    }
    args.push("--max-runs".into());
    args.push(options.max_runs.to_string());
    args.push("--timeout".into());
    args.push(options.timeout.to_string());
    if let Some(t) = options.hard_timeout {
        args.push("--hard-timeout".into());
        args.push(t.to_string());
    }
    if let Some(t) = options.shutdown_timeout {
        args.push("--shutdown-timeout".into());
        args.push(t.to_string());
    }
    if options.stop_on_failure == Some(false) {
        args.push("--no-stop-on-failure".into());
    }
    if options.deploy == Some(false) {
        args.push("--no-deploy".into());
    }
    if options.build == Some(false) {
        args.push("--no-build".into());
    }
    push_optional_value_arg(&mut args, "--display", options.display);
    if options.capture_on_failure {
        args.push("--capture-on-failure".into());
    }
    push_optional_value_arg(&mut args, "--filter-pattern", options.filter_pattern);
    push_optional_value_arg(&mut args, "--output-file", options.output_file);
    push_optional_equals_arg(&mut args, "--host-args", options.host_args);
    push_optional_equals_arg(&mut args, "--vm-args", options.vm_args);
    if options.strace {
        args.push("--strace".into());
    }
    args
}

fn build_cluster_cell_test_args(cell: &ClusterCell, max_runs: u32, timeout: u64) -> Vec<String> {
    build_cluster_test_args(ClusterTestArgOptions {
        network: cell.network.as_deref(),
        players: cell.players,
        max_runs,
        timeout,
        hard_timeout: None,
        shutdown_timeout: None,
        stop_on_failure: None,
        deploy: None,
        build: None,
        display: cell.display.as_deref(),
        capture_on_failure: cell.capture_on_failure.unwrap_or(false),
        filter_pattern: None,
        output_file: None,
        host_args: cell.host_args.as_deref(),
        vm_args: cell.vm_args.as_deref(),
        strace: false,
    })
}

fn build_cluster_test_input_args(
    input: &ClusterTestInput,
    max_runs: u32,
    timeout: u64,
) -> Vec<String> {
    build_cluster_test_args(ClusterTestArgOptions {
        network: input.network.as_deref(),
        players: input.players,
        max_runs,
        timeout,
        hard_timeout: input.hard_timeout,
        shutdown_timeout: input.shutdown_timeout,
        stop_on_failure: input.stop_on_failure,
        deploy: input.deploy,
        build: input.build,
        display: input.display.as_deref(),
        capture_on_failure: input.capture_on_failure.unwrap_or(false),
        filter_pattern: input.filter_pattern.as_deref(),
        output_file: input.output_file.as_deref(),
        host_args: input.host_args.as_deref(),
        vm_args: input.vm_args.as_deref(),
        strace: input.strace.unwrap_or(false),
    })
}

fn cluster_command_header(ctx: &ClusterContext, args: &[String]) -> String {
    format!(
        "=== fragpipe-mcp (cluster proxy) ===\nworkdir: {}\ncommand: {} --cluster {} --vm-count {} {}\n\n",
        ctx.workdir.display(),
        ctx.cluster_ctl_bin,
        ctx.cluster,
        ctx.vm_count,
        args.join(" "),
    )
}

fn push_common_fragpipe_args(
    args: &mut Vec<String>,
    common: &CommonInput,
    max_runs: u32,
    timeout: u64,
) {
    if let Some(config) = &common.config {
        args.push("--config".into());
        args.push(config.clone());
    }
    args.push("--max-runs".into());
    args.push(max_runs.to_string());
    args.push("--timeout".into());
    args.push(timeout.to_string());
    if common.stop_on_failure == Some(false) {
        args.push("--stop-on-failure=false".into());
    }
    if common.no_build.unwrap_or(false) {
        args.push("--no-build".into());
    }
    if common.dry_run.unwrap_or(false) {
        args.push("--dry-run".into());
    }
}

fn command_header(ctx: &CommandContext, args: &[String]) -> String {
    format!(
        "=== fragpipe-mcp ===\nworkdir: {}\ncommand: {} {}\n\n",
        ctx.workdir.display(),
        ctx.fragpipe_bin,
        args.join(" ")
    )
}

fn terminate_process_group(pgid: u32) {
    #[cfg(unix)]
    {
        let pid = pgid.to_string();
        let _ = Command::new("kill")
            .args(["-TERM", &format!("-{pid}")])
            .status();
        std::thread::sleep(Duration::from_secs(2));
        let _ = Command::new("kill")
            .args(["-KILL", &format!("-{pid}")])
            .status();
    }
    #[cfg(not(unix))]
    {
        let _ = pgid;
    }
}

#[tool_router]
impl FragpipeMcp {
    #[tool(description = "Run native WebRTC Direct 1v1 through fragpipe.")]
    fn repeat_webrtc_1v1(
        &self,
        Parameters(input): Parameters<RunInput>,
    ) -> Result<CallToolResult, ErrorData> {
        self.run_single("webrtc-1v1", input, 300)
    }

    #[tool(description = "Run forced-relay internet 1v1 through fragpipe.")]
    fn repeat_internet_1v1(
        &self,
        Parameters(input): Parameters<InternetRunInput>,
    ) -> Result<CallToolResult, ErrorData> {
        self.run_internet(input, 200)
    }

    #[tool(description = "Run desktop plus Android test-peer WebRTC Direct 1v1 through fragpipe.")]
    fn repeat_android_1v1(
        &self,
        Parameters(input): Parameters<AndroidRunInput>,
    ) -> Result<CallToolResult, ErrorData> {
        let (output, _) = self.run_android("android-1v1", input, 300)?;
        Ok(CallToolResult::success(vec![Content::text(output)]))
    }

    #[tool(
        description = "Run Android full-app UI launch, landscape assertion, and screenshot through fragpipe."
    )]
    fn repeat_android_ui(
        &self,
        Parameters(input): Parameters<AndroidRunInput>,
    ) -> Result<CallToolResult, ErrorData> {
        let (output, _) = self.run_android("android-ui", input, 120)?;
        Ok(CallToolResult::success(vec![Content::text(output)]))
    }

    #[tool(description = "Run fragpipe android-doctor against an Android config.")]
    fn android_doctor(
        &self,
        Parameters(input): Parameters<AndroidDoctorInput>,
    ) -> Result<CallToolResult, ErrorData> {
        let mut args = vec!["android-doctor".to_string()];
        if let Some(config) = &input.common.config {
            args.push("--config".into());
            args.push(config.clone());
        }
        if input.device.unwrap_or(false) {
            args.push("--device".into());
        }
        if let Some(serial) = input.adb_serial {
            args.push("--adb-serial".into());
            args.push(serial);
        }
        let (output, _) =
            self.run_fragpipe(&input.common, args, input.common.timeout.unwrap_or(120))?;
        Ok(CallToolResult::success(vec![Content::text(output)]))
    }

    #[tool(
        description = "Run a cross-platform 1v1 fragpipe matrix: native WebRTC, Android 1v1, and Android UI."
    )]
    fn repeat_cross_platform_1v1(
        &self,
        Parameters(input): Parameters<CrossPlatformInput>,
    ) -> Result<CallToolResult, ErrorData> {
        let max_runs = input.common.max_runs.unwrap_or(1).min(50);
        let timeout = input.common.timeout.unwrap_or(300);
        let stop_on_failure = input.common.stop_on_failure.unwrap_or(true);
        let mut output = String::new();
        let mut failed = 0u32;

        if input.native_webrtc.unwrap_or(true) {
            let mut common = CommonInput {
                config: input
                    .native_config
                    .clone()
                    .or_else(|| input.common.config.clone()),
                ..input.common.clone()
            };
            common.max_runs = Some(max_runs);
            common.timeout = Some(timeout);
            let mut args = vec!["webrtc-1v1".to_string()];
            push_common_fragpipe_args(&mut args, &common, max_runs, timeout);
            let (text, ok) = self.run_fragpipe(&common, args, timeout * max_runs as u64 + 120)?;
            output.push_str("=== MATRIX CELL: native-webrtc ===\n");
            output.push_str(&text);
            if !ok {
                failed += 1;
                if stop_on_failure {
                    output.push_str(&format!(
                        "\n=== CROSS-PLATFORM SUMMARY ===\nfailed cells: {failed}\n"
                    ));
                    return Ok(CallToolResult::success(vec![Content::text(output)]));
                }
            }
        }

        let mut early_exit = false;
        for (enabled, command, label, timeout_default) in [
            (
                input.android_1v1.unwrap_or(true),
                "android-1v1",
                "android-1v1",
                300,
            ),
            (
                input.android_ui.unwrap_or(true),
                "android-ui",
                "android-ui",
                120,
            ),
        ] {
            if !enabled {
                continue;
            }
            let common = CommonInput {
                config: input
                    .android_config
                    .clone()
                    .or_else(|| input.common.config.clone()),
                timeout: Some(input.common.timeout.unwrap_or(timeout_default)),
                max_runs: Some(max_runs),
                ..input.common.clone()
            };
            let android_input = AndroidRunInput {
                common,
                no_install: input.no_install,
                device: input.device,
                adb_serial: input.adb_serial.clone(),
                launch_config: None,
            };
            let (text, ok) = self.run_android(command, android_input, timeout_default)?;
            output.push_str(&format!("=== MATRIX CELL: {label} ===\n"));
            output.push_str(&text);
            if !ok {
                failed += 1;
                if stop_on_failure {
                    early_exit = true;
                    break;
                }
            }
        }

        if !early_exit && let Some(cells) = input.cluster_cells.as_ref() {
            for cell in cells {
                let label = cell.label.clone().unwrap_or_else(|| cell.cluster.clone());
                let cell_timeout = cell.timeout.unwrap_or(timeout);

                if let Some(target) = &cell.nix_up_target {
                    let flake = cell.nix_flake.as_deref().unwrap_or(".");
                    let (up_text, up_ok) =
                        self.run_nix_app(flake, target, &input.common.workdir, 900)?;
                    output.push_str(&format!("=== MATRIX CELL: {label} (up) ===\n"));
                    output.push_str(&up_text);
                    if !up_ok {
                        failed += 1;
                        if stop_on_failure {
                            break;
                        }
                        continue;
                    }
                }

                let cluster_common = ClusterCommonInput {
                    cluster_ctl_bin: input.cluster_ctl_bin.clone(),
                    workdir: input.common.workdir.clone(),
                    cluster: Some(cell.cluster.clone()),
                    vm_count: cell.vm_count,
                    mcp_timeout: None,
                };

                let args = build_cluster_cell_test_args(cell, max_runs, cell_timeout);

                let wall = cell_timeout
                    .saturating_mul(max_runs.max(1) as u64)
                    .saturating_add(600);
                let (text, ok) = self.run_cluster_ctl(&cluster_common, args, wall)?;
                output.push_str(&format!("=== MATRIX CELL: {label} ===\n"));
                output.push_str(&text);
                if !ok {
                    failed += 1;
                    if stop_on_failure {
                        break;
                    }
                }
            }
        }

        output.push_str(&format!(
            "\n=== CROSS-PLATFORM SUMMARY ===\nfailed cells: {failed}\n"
        ));
        Ok(CallToolResult::success(vec![Content::text(output)]))
    }

    // ── Cluster proxy tools (delegate to `cluster-ctl`) ─────────────────────

    #[tool(
        description = "Check status of all VMs in the cluster. Shells out to `cluster-ctl status` and reports SSH reachability, VM/Steam/game processes, and lease holders."
    )]
    fn cluster_status(
        &self,
        Parameters(input): Parameters<ClusterStatusInput>,
    ) -> Result<CallToolResult, ErrorData> {
        let timeout = input.common.mcp_timeout.unwrap_or(60);
        let (output, _) = self.run_cluster_ctl(&input.common, vec!["status".into()], timeout)?;
        Ok(CallToolResult::success(vec![Content::text(output)]))
    }

    #[tool(
        description = "Read game logs from cluster VMs. Shells out to `cluster-ctl logs`. Supports per-VM targeting, head/tail mode, and regex filtering."
    )]
    fn cluster_logs(
        &self,
        Parameters(input): Parameters<ClusterLogsInput>,
    ) -> Result<CallToolResult, ErrorData> {
        let timeout = input.common.mcp_timeout.unwrap_or(120);
        let mut args: Vec<String> = vec!["logs".into()];
        if let Some(vm) = &input.vm {
            args.push(vm.clone());
        }
        if let Some(n) = input.max_lines {
            args.push("--lines".into());
            args.push(n.to_string());
        }
        // Steampipe defaults to tail; only pass `--head` when explicitly requested.
        if input.tail == Some(false) {
            args.push("--head".into());
        }
        if let Some(p) = &input.pattern {
            args.push("--pattern".into());
            args.push(p.clone());
        }
        let (output, _) = self.run_cluster_ctl(&input.common, args, timeout)?;
        Ok(CallToolResult::success(vec![Content::text(output)]))
    }

    #[tool(
        description = "Build and deploy the game binary + assets to cluster VMs. Shells out to `cluster-ctl deploy`."
    )]
    fn cluster_deploy(
        &self,
        Parameters(input): Parameters<ClusterDeployInput>,
    ) -> Result<CallToolResult, ErrorData> {
        let timeout = input.common.mcp_timeout.unwrap_or(900);
        let mut args: Vec<String> = vec!["deploy".into()];
        if input.build == Some(false) {
            args.push("--no-build".into());
        }
        if input.verify.unwrap_or(false) {
            args.push("--verify".into());
        }
        let (output, _) = self.run_cluster_ctl(&input.common, args, timeout)?;
        Ok(CallToolResult::success(vec![Content::text(output)]))
    }

    #[tool(
        description = "Kill the game process on all cluster VMs. Shells out to `cluster-ctl stop-game`. Optionally also stops Steam and the compositor."
    )]
    fn cluster_stop(
        &self,
        Parameters(input): Parameters<ClusterStopInput>,
    ) -> Result<CallToolResult, ErrorData> {
        let timeout = input.common.mcp_timeout.unwrap_or(60);
        let mut args: Vec<String> = vec!["stop-game".into()];
        if input.kill_steam.unwrap_or(false) {
            args.push("--kill-steam".into());
        }
        let (output, _) = self.run_cluster_ctl(&input.common, args, timeout)?;
        Ok(CallToolResult::success(vec![Content::text(output)]))
    }

    #[tool(
        description = "Run end-to-end tests on the VM cluster. Shells out to `cluster-ctl test`. Handles deploy, launch, monitoring, and pass/fail reporting per run."
    )]
    fn cluster_test(
        &self,
        Parameters(input): Parameters<ClusterTestInput>,
    ) -> Result<CallToolResult, ErrorData> {
        let per_run_timeout = input.timeout.unwrap_or(300);
        let max_runs = input.max_runs.unwrap_or(1).min(50);
        // Wall-clock budget for the whole session: explicit override, else derived from
        // `per_run_timeout * max_runs` plus a buffer for deploy and per-run overhead.
        let wall_timeout = input.common.mcp_timeout.unwrap_or_else(|| {
            per_run_timeout
                .saturating_mul(max_runs.max(1) as u64)
                .saturating_add(600)
        });

        let args = build_cluster_test_input_args(&input, max_runs, per_run_timeout);

        let (output, _) = self.run_cluster_ctl(&input.common, args, wall_timeout)?;
        Ok(CallToolResult::success(vec![Content::text(output)]))
    }

    #[tool(
        description = "Show cluster test history (pass/fail rates, exit-code breakdown). Shells out to `cluster-ctl history`."
    )]
    fn cluster_history(
        &self,
        Parameters(input): Parameters<ClusterHistoryInput>,
    ) -> Result<CallToolResult, ErrorData> {
        let timeout = input.common.mcp_timeout.unwrap_or(30);
        let mut args: Vec<String> = vec!["history".into()];
        if let Some(n) = input.last {
            args.push("--last".into());
            args.push(n.to_string());
        }
        let (output, _) = self.run_cluster_ctl(&input.common, args, timeout)?;
        Ok(CallToolResult::success(vec![Content::text(output)]))
    }

    #[tool(
        description = "Run a Nix flake app. Executes `nix run <flake>#<target> -- <args...>` in the configured workdir."
    )]
    fn nix_run(
        &self,
        Parameters(input): Parameters<NixRunInput>,
    ) -> Result<CallToolResult, ErrorData> {
        let flake = input.flake.as_deref().unwrap_or(".");
        let target = input.target.as_str();
        let timeout_secs = input.timeout.unwrap_or(300);
        let workdir_override = input.workdir.clone();
        let extra_args = input.args.as_deref().unwrap_or(&[]).to_vec();
        let (text, _) =
            self.run_nix_app_inner(flake, target, &workdir_override, timeout_secs, &extra_args)?;
        Ok(CallToolResult::success(vec![Content::text(text)]))
    }
}

impl FragpipeMcp {
    fn run_nix_app(
        &self,
        flake: &str,
        target: &str,
        workdir_override: &Option<String>,
        timeout_secs: u64,
    ) -> Result<(String, bool), ErrorData> {
        self.run_nix_app_inner(flake, target, workdir_override, timeout_secs, &[])
    }

    fn run_nix_app_inner(
        &self,
        flake: &str,
        target: &str,
        workdir_override: &Option<String>,
        timeout_secs: u64,
        extra_args: &[String],
    ) -> Result<(String, bool), ErrorData> {
        let flake_ref = format!("{flake}#{target}");
        let workdir = workdir_override
            .clone()
            .map(PathBuf::from)
            .unwrap_or_else(|| self.default_workdir.clone());

        let mut cmd = Command::new("nix");
        cmd.arg("run").arg(&flake_ref);
        if !extra_args.is_empty() {
            cmd.arg("--");
            cmd.args(extra_args);
        }
        cmd.current_dir(&workdir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt as _;
            cmd.process_group(0);
        }

        let child = cmd.spawn().map_err(|error| {
            ErrorData::internal_error(format!("failed to spawn nix run: {error}"), None)
        })?;
        let mut child = ManagedChild::new(child);

        let started = Instant::now();
        loop {
            if let Some(status) = child.try_wait().map_err(|error| {
                ErrorData::internal_error(format!("failed to poll nix run: {error}"), None)
            })? {
                let output = child.wait_with_output().map_err(|error| {
                    ErrorData::internal_error(
                        format!("failed to collect nix run output: {error}"),
                        None,
                    )
                })?;
                let mut text = format!(
                    "=== fragpipe-mcp (nix proxy) ===\nworkdir: {}\ncommand: nix run {flake_ref}\n\n",
                    workdir.display()
                );
                text.push_str(&String::from_utf8_lossy(&output.stdout));
                text.push_str(&String::from_utf8_lossy(&output.stderr));
                let ok = status.success();
                text.push_str(if ok {
                    "\n--- PASS ---\n"
                } else {
                    "\n--- FAIL ---\n"
                });
                return Ok((text, ok));
            }
            if started.elapsed() >= Duration::from_secs(timeout_secs) {
                child.terminate();
                let output = child.wait_with_output().map_err(|error| {
                    ErrorData::internal_error(
                        format!("failed to collect timed-out nix run output: {error}"),
                        None,
                    )
                })?;
                let mut text = format!(
                    "=== fragpipe-mcp (nix proxy) ===\nworkdir: {}\ncommand: nix run {flake_ref}\n\n",
                    workdir.display()
                );
                text.push_str(&String::from_utf8_lossy(&output.stdout));
                text.push_str(&String::from_utf8_lossy(&output.stderr));
                text.push_str(&format!("\n--- TIMEOUT ({timeout_secs}s limit) ---\n"));
                return Ok((text, false));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

#[tool_handler]
impl ServerHandler for FragpipeMcp {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::default();
        info.protocol_version = ProtocolVersion::V_2024_11_05;
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info.server_info = Implementation::from_build_env();
        info.instructions = Some(
            "Run fragpipe smoke and fix-loop commands plus cluster proxy operations. \
             Native tools (webrtc-1v1, internet-1v1, android-*) shell out to the configured `fragpipe` \
             binary; cluster_* tools shell out to `cluster-ctl` (steampipe). Pass \
             workdir/config/cluster explicitly for project-specific runs."
                .into(),
        );
        info
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    if std::env::args().any(|arg| arg == "--help" || arg == "-h") {
        println!(
            "fragpipe-mcp\n\nMCP stdio server for fragpipe.\n\nRun without arguments from an MCP client."
        );
        return Ok(());
    }

    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    let service = FragpipeMcp::new().serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn has_split_flag_value(args: &[String], flag: &str, value: &str) -> bool {
        args.windows(2)
            .any(|window| window[0] == flag && window[1] == value)
    }

    #[test]
    fn cluster_test_args_forward_host_and_vm_args_with_equals_form() {
        let host_args = "--auto-host-udp 0.0.0.0:7777";
        let vm_args = "--auto-join-udp 10.0.0.1:7777";
        let args = build_cluster_test_args(ClusterTestArgOptions {
            network: Some("lan"),
            players: Some(2),
            max_runs: 3,
            timeout: 120,
            hard_timeout: None,
            shutdown_timeout: None,
            stop_on_failure: None,
            deploy: None,
            build: None,
            display: Some("headless"),
            capture_on_failure: true,
            filter_pattern: None,
            output_file: None,
            host_args: Some(host_args),
            vm_args: Some(vm_args),
            strace: false,
        });

        assert!(args.contains(&format!("--host-args={host_args}")));
        assert!(args.contains(&format!("--vm-args={vm_args}")));
        assert!(!args.contains(&"--host-args".to_string()));
        assert!(!args.contains(&"--vm-args".to_string()));
        assert!(!has_split_flag_value(&args, "--host-args", host_args));
        assert!(!has_split_flag_value(&args, "--vm-args", vm_args));
    }

    #[test]
    fn cluster_test_args_preserve_empty_host_and_vm_args() {
        let args = build_cluster_test_args(ClusterTestArgOptions {
            network: None,
            players: None,
            max_runs: 1,
            timeout: 300,
            hard_timeout: None,
            shutdown_timeout: None,
            stop_on_failure: None,
            deploy: None,
            build: None,
            display: None,
            capture_on_failure: false,
            filter_pattern: None,
            output_file: None,
            host_args: Some(""),
            vm_args: Some(""),
            strace: false,
        });

        assert!(args.contains(&"--host-args=".to_string()));
        assert!(args.contains(&"--vm-args=".to_string()));
    }
}
