pub mod android;
pub mod android_worker;
pub mod config;
pub mod direct;
pub mod logwatch;
pub mod process;
pub mod runner;
pub mod ssh;
pub mod util;
pub mod webrtc;

use std::net::IpAddr;
use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};

use direct::{
    DirectRunOptions, OutputFormat as DirectOutputFormat, TournamentRunOptions, run_direct_1v1,
    run_direct_tournament,
};
use runner::{
    AndroidDoctorOptions, AndroidRunOptions, AndroidUiRunOptions, InternetRunOptions, OutputFormat,
    ShipOptions, WebRtcRunOptions, run_android_1v1, run_android_doctor, run_android_ui,
    run_internet_1v1, run_ship, run_webrtc_1v1,
};

#[derive(Debug, Parser)]
#[command(name = "fragpipe")]
#[command(about = "Generic bare-metal multiplayer test orchestration")]
pub struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Build and deploy the game binary + assets to a remote peer, with optional restart.
    #[command(name = "ship")]
    Ship(ShipArgs),
    /// Run a native WebRTC Direct 1v1 smoke test.
    #[command(name = "webrtc-1v1")]
    Webrtc1v1(WebRtc1v1Args),
    /// Run a physical-peer LAN/UDP 1v1 smoke test through SSH.
    #[command(name = "direct-1v1")]
    Direct1v1(Direct1v1Args),
    /// Run a multi-process physical-peer LAN tournament through SSH.
    #[command(name = "direct-tournament")]
    DirectTournament(DirectTournamentArgs),
    /// Run the forced-relay internet 1v1 smoke test.
    #[command(name = "internet-1v1")]
    Internet1v1(Internet1v1Args),
    /// Run a desktop-listener + Android-emulator joiner WebRTC 1v1 smoke test.
    #[command(name = "android-1v1")]
    Android1v1(Android1v1Args),
    /// Launch the Android app, capture a screenshot, and validate landscape UI.
    #[command(name = "android-ui")]
    AndroidUi(AndroidUiArgs),
    /// Validate Android SDK/adb/APK/manifest prerequisites.
    #[command(name = "android-doctor")]
    AndroidDoctor(AndroidDoctorArgs),
    /// Lease remote Android slots and run a local command against their ADB server.
    #[command(name = "android-with")]
    AndroidWith(AndroidWithArgs),
    /// Receive restricted SSH lifecycle and rsync requests from a forced command.
    #[command(name = "ssh-gate", hide = true)]
    SshGate(SshGateArgs),
}

#[derive(Debug, Parser)]
struct SshGateArgs {
    /// Fixed directory containing every file the key may access.
    #[arg(long)]
    root: PathBuf,

    /// Fixed executable path relative to root.
    #[arg(long)]
    binary: PathBuf,

    /// Fixed log path relative to root.
    #[arg(long, default_value = "game.log")]
    log_file: PathBuf,

    /// Fixed PID path relative to root.
    #[arg(long, default_value = ".fragpipe.pid")]
    pid_file: PathBuf,

    /// Absolute path to the restricted rsync wrapper.
    #[arg(long)]
    rrsync: PathBuf,
}

#[derive(Debug, Parser)]
struct ShipArgs {
    /// Project config path.
    #[arg(long, default_value = "fragpipe.toml")]
    config: PathBuf,

    /// Remote peer name from the config.
    #[arg(long, required = true)]
    remote: String,

    /// Skip the configured build command.
    #[arg(long)]
    no_build: bool,

    /// Skip binary/assets deployment.
    #[arg(long)]
    no_deploy: bool,

    /// Stop the old process and launch a new instance after deploy.
    #[arg(long)]
    restart: bool,

    /// Override launch args for --restart (otherwise uses remote.join_args).
    #[arg(long, num_args = 1.., allow_hyphen_values = true)]
    launch_args: Option<Vec<String>>,

    /// Print commands without building, SSHing or rsyncing.
    #[arg(long)]
    dry_run: bool,
}

#[derive(Debug, Parser)]
struct Internet1v1Args {
    /// Project config path.
    #[arg(long, default_value = "fragpipe.toml")]
    config: PathBuf,

    /// Number of test runs.
    #[arg(long)]
    max_runs: Option<u32>,

    /// Stop after the first failed run.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    stop_on_failure: bool,

    /// Chessbender repository root containing dev/netns/internet-1v1-forced-relay.sh.
    #[arg(long)]
    workdir: Option<PathBuf>,

    /// Chessbender binary passed as GAME_BIN.
    #[arg(long)]
    game_bin: Option<PathBuf>,

    /// thespan-rendezvous binary passed as RDV_BIN.
    #[arg(long)]
    rdv_bin: Option<PathBuf>,

    /// Asset root passed as ASSET_ROOT. Defaults to <workdir>/assets.
    #[arg(long)]
    asset_root: Option<PathBuf>,

    /// Log directory passed as LOG_DIR. Defaults under <workdir>/logs/fragpipe.
    #[arg(long)]
    log_dir: Option<PathBuf>,

    /// Overall smoke timeout in seconds passed as TIMEOUT_SECS.
    #[arg(long)]
    timeout_secs: Option<u64>,

    /// Pass marker passed as PASS_MARKER.
    #[arg(long)]
    pass_marker: Option<String>,

    /// Print command and env without launching the smoke.
    #[arg(long)]
    dry_run: bool,

    /// Output format.
    #[arg(long, default_value = "text")]
    output_format: CliOutputFormat,
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

    /// Skip binary/assets deployment for remote mode.
    #[arg(long)]
    no_deploy: bool,

    /// Remote peer name from the config. Omit for local multi-process mode.
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
    output_format: CliOutputFormat,
}

#[derive(Debug, Parser)]
struct Direct1v1Args {
    /// Project config path.
    #[arg(long, default_value = "fragpipe.toml")]
    config: PathBuf,

    /// Remote peer name from the config.
    #[arg(long, required = true)]
    remote: String,

    /// Number of test runs.
    #[arg(long)]
    max_runs: Option<u32>,

    /// Per-run timeout in seconds.
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

    /// Local IP the remote peer should dial.
    #[arg(long)]
    local_ip: Option<IpAddr>,

    /// UDP listen port.
    #[arg(long)]
    port: Option<u16>,

    /// Add --headless to both peers.
    #[arg(long)]
    headless: bool,

    /// Transport selector retained for MCP compatibility; only lan is supported.
    #[arg(long, default_value = "lan")]
    transport: String,

    /// Print commands without launching or SSHing.
    #[arg(long)]
    dry_run: bool,

    /// Output format.
    #[arg(long, default_value = "text")]
    output_format: CliOutputFormat,
}

#[derive(Debug, Parser)]
struct DirectTournamentArgs {
    /// Project config path.
    #[arg(long, default_value = "fragpipe.toml")]
    config: PathBuf,

    /// Remote peer name from the config.
    #[arg(long, required = true)]
    remote: String,

    /// Number of test runs.
    #[arg(long)]
    max_runs: Option<u32>,

    /// Per-run timeout in seconds.
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

    /// Reachable local IP the remote peers should dial.
    #[arg(long)]
    local_ip: Option<IpAddr>,

    /// Print commands without launching or SSHing.
    #[arg(long)]
    dry_run: bool,

    /// Output format.
    #[arg(long, default_value = "text")]
    output_format: CliOutputFormat,
}

#[derive(Debug, Parser)]
struct Android1v1Args {
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

    /// Skip `adb install -r` (assume the APK is already on the emulator).
    #[arg(long)]
    no_install: bool,

    /// Reachable local IP used when the WebRTC listen address is wildcard or loopback.
    /// On Android emulators 10.0.2.2 maps back to the host, which fragpipe rewrites
    /// the listener's multiaddr to.
    #[arg(long)]
    local_ip: Option<IpAddr>,

    /// Local WebRTC listen port.
    #[arg(long)]
    webrtc_port: Option<u16>,

    /// Print commands without launching adb / emulator.
    #[arg(long)]
    dry_run: bool,

    /// Override adb serial for a physical device or specific emulator.
    #[arg(long)]
    adb_serial: Option<String>,

    /// Use a physical device instead of booting the configured AVD.
    #[arg(long)]
    device: bool,

    /// Named remote Android slot from the worker config.
    #[arg(long)]
    slot: Option<String>,

    /// Android worker config path. Defaults to $XDG_CONFIG_HOME/fragpipe/workers.toml.
    #[arg(long)]
    workers_config: Option<PathBuf>,

    /// Output format.
    #[arg(long, default_value = "text")]
    output_format: CliOutputFormat,
}

#[derive(Debug, Parser)]
struct AndroidUiArgs {
    /// Project config path.
    #[arg(long, default_value = "fragpipe.toml")]
    config: PathBuf,

    /// Number of test runs.
    #[arg(long)]
    max_runs: Option<u32>,

    /// Timeout per UI run in seconds.
    #[arg(long)]
    timeout: Option<u64>,

    /// Stop after the first failed run.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    stop_on_failure: bool,

    /// Skip the configured game and APK build commands.
    #[arg(long)]
    no_build: bool,

    /// Skip `adb install -r`.
    #[arg(long)]
    no_install: bool,

    /// Override adb serial for a physical device or specific emulator.
    #[arg(long)]
    adb_serial: Option<String>,

    /// Use a physical device instead of booting the configured AVD.
    #[arg(long)]
    device: bool,

    /// Named remote Android slot from the worker config.
    #[arg(long)]
    slot: Option<String>,

    /// Android worker config path. Defaults to $XDG_CONFIG_HOME/fragpipe/workers.toml.
    #[arg(long)]
    workers_config: Option<PathBuf>,

    /// JSON launch config pushed to the Android device before starting.
    #[arg(long)]
    launch_config: Option<String>,

    /// Visual fixture catalog selection (`all` or a `*`/`?` glob).
    #[arg(long, value_name = "ALL|GLOB")]
    visual_fixtures: Option<String>,

    /// Print commands without launching adb / emulator.
    #[arg(long)]
    dry_run: bool,

    /// Output format.
    #[arg(long, default_value = "text")]
    output_format: CliOutputFormat,
}

#[derive(Debug, Parser)]
struct AndroidDoctorArgs {
    /// Project config path.
    #[arg(long, default_value = "fragpipe.toml")]
    config: PathBuf,

    /// Override adb serial for a physical device or specific emulator.
    #[arg(long)]
    adb_serial: Option<String>,

    /// Use a physical device instead of requiring the configured AVD.
    #[arg(long)]
    device: bool,

    /// Named remote Android slot from the worker config.
    #[arg(long)]
    slot: Option<String>,

    /// Android worker config path. Defaults to $XDG_CONFIG_HOME/fragpipe/workers.toml.
    #[arg(long)]
    workers_config: Option<PathBuf>,
}

#[derive(Debug, Parser)]
struct AndroidWithArgs {
    /// Named Android slots. Multiple slots must belong to one worker.
    #[arg(long, required = true)]
    slot: Vec<String>,

    /// Android worker config path. Defaults to $XDG_CONFIG_HOME/fragpipe/workers.toml.
    #[arg(long)]
    workers_config: Option<PathBuf>,

    /// Print lifecycle actions without SSHing or running the command.
    #[arg(long)]
    dry_run: bool,

    /// Command to run with the leased ADB server and serial environment.
    #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
    command: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum CliOutputFormat {
    Text,
    Jsonl,
}

impl From<CliOutputFormat> for OutputFormat {
    fn from(value: CliOutputFormat) -> Self {
        match value {
            CliOutputFormat::Text => Self::Text,
            CliOutputFormat::Jsonl => Self::Jsonl,
        }
    }
}

pub fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Commands::Ship(args) => run_ship(ShipOptions {
            config_path: args.config,
            remote: args.remote,
            no_build: args.no_build,
            no_deploy: args.no_deploy,
            restart: args.restart,
            launch_args: args.launch_args,
            dry_run: args.dry_run,
        }),
        Commands::Webrtc1v1(args) => run_webrtc_1v1(WebRtcRunOptions {
            config_path: args.config,
            max_runs: args.max_runs,
            timeout_secs: args.timeout,
            stop_on_failure: args.stop_on_failure,
            no_build: args.no_build,
            no_deploy: args.no_deploy,
            remote: args.remote,
            local_ip: args.local_ip,
            webrtc_port: args.webrtc_port,
            dry_run: args.dry_run,
            output_format: args.output_format.into(),
        }),
        Commands::Direct1v1(args) => run_direct_1v1(DirectRunOptions {
            config_path: args.config,
            remote: args.remote,
            max_runs: args.max_runs,
            timeout_secs: args.timeout,
            stop_on_failure: args.stop_on_failure,
            no_build: args.no_build,
            no_deploy: args.no_deploy,
            local_ip: args.local_ip,
            port: args.port,
            headless: args.headless,
            dry_run: args.dry_run,
            output_format: match args.output_format {
                CliOutputFormat::Text => DirectOutputFormat::Text,
                CliOutputFormat::Jsonl => DirectOutputFormat::Jsonl,
            },
            transport: Some(args.transport),
        }),
        Commands::DirectTournament(args) => run_direct_tournament(TournamentRunOptions {
            config_path: args.config,
            remote: args.remote,
            max_runs: args.max_runs,
            timeout_secs: args.timeout,
            stop_on_failure: args.stop_on_failure,
            no_build: args.no_build,
            no_deploy: args.no_deploy,
            local_ip: args.local_ip,
            dry_run: args.dry_run,
            output_format: match args.output_format {
                CliOutputFormat::Text => DirectOutputFormat::Text,
                CliOutputFormat::Jsonl => DirectOutputFormat::Jsonl,
            },
        }),
        Commands::Internet1v1(args) => run_internet_1v1(InternetRunOptions {
            config_path: args.config,
            max_runs: args.max_runs,
            timeout_secs: args.timeout_secs,
            stop_on_failure: args.stop_on_failure,
            workdir: args.workdir,
            game_bin: args.game_bin,
            rdv_bin: args.rdv_bin,
            asset_root: args.asset_root,
            log_dir: args.log_dir,
            pass_marker: args.pass_marker,
            dry_run: args.dry_run,
            output_format: args.output_format.into(),
        }),
        Commands::Android1v1(args) => run_android_1v1(AndroidRunOptions {
            config_path: args.config,
            max_runs: args.max_runs,
            timeout_secs: args.timeout,
            stop_on_failure: args.stop_on_failure,
            no_build: args.no_build,
            no_install: args.no_install,
            local_ip: args.local_ip,
            webrtc_port: args.webrtc_port,
            dry_run: args.dry_run,
            adb_serial: args.adb_serial,
            device: args.device,
            slot: args.slot,
            workers_config: args.workers_config,
            launch_config: None,
            output_format: args.output_format.into(),
        }),
        Commands::AndroidUi(args) => run_android_ui(AndroidUiRunOptions {
            config_path: args.config,
            max_runs: args.max_runs,
            timeout_secs: args.timeout,
            stop_on_failure: args.stop_on_failure,
            no_build: args.no_build,
            no_install: args.no_install,
            dry_run: args.dry_run,
            adb_serial: args.adb_serial,
            device: args.device,
            slot: args.slot,
            workers_config: args.workers_config,
            launch_config: args.launch_config,
            visual_fixtures: args.visual_fixtures,
            output_format: args.output_format.into(),
        }),
        Commands::AndroidDoctor(args) => run_android_doctor(AndroidDoctorOptions {
            config_path: args.config,
            adb_serial: args.adb_serial,
            device: args.device,
            slot: args.slot,
            workers_config: args.workers_config,
        }),
        Commands::AndroidWith(args) => android_worker::run_android_with(
            args.workers_config.as_deref(),
            &args.slot,
            &args.command,
            args.dry_run,
        ),
        Commands::SshGate(args) => ssh::run_gate(
            &args.root,
            &args.binary,
            &args.log_file,
            &args.pid_file,
            &args.rrsync,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_webrtc_1v1_subcommand() {
        let cli = Cli::try_parse_from(["fragpipe", "webrtc-1v1", "--config", "test.toml"]).unwrap();
        assert!(matches!(cli.command, Commands::Webrtc1v1(_)));
    }

    #[test]
    fn parse_internet_1v1_subcommand() {
        let cli = Cli::try_parse_from(["fragpipe", "internet-1v1"]).unwrap();
        assert!(matches!(cli.command, Commands::Internet1v1(_)));
    }

    #[test]
    fn direct_1v1_defaults_to_lan() {
        let cli = Cli::try_parse_from(["fragpipe", "direct-1v1", "--remote", "nomad"]).unwrap();
        if let Commands::Direct1v1(args) = cli.command {
            assert_eq!(args.config, PathBuf::from("fragpipe.toml"));
            assert_eq!(args.remote, "nomad");
            assert_eq!(args.transport, "lan");
            assert!(!args.headless);
            assert!(!args.dry_run);
        } else {
            panic!("expected Direct1v1 variant");
        }
    }

    #[test]
    fn direct_1v1_forwards_lan_flags() {
        let cli = Cli::try_parse_from([
            "fragpipe",
            "direct-1v1",
            "--config",
            "game.toml",
            "--remote",
            "nomad",
            "--max-runs",
            "3",
            "--timeout",
            "90",
            "--no-build",
            "--no-deploy",
            "--local-ip",
            "10.10.0.1",
            "--port",
            "27100",
            "--headless",
            "--transport",
            "lan",
            "--dry-run",
            "--output-format",
            "jsonl",
        ])
        .unwrap();
        if let Commands::Direct1v1(args) = cli.command {
            assert_eq!(args.config, PathBuf::from("game.toml"));
            assert_eq!(args.max_runs, Some(3));
            assert_eq!(args.timeout, Some(90));
            assert!(args.no_build);
            assert!(args.no_deploy);
            assert_eq!(args.local_ip, Some("10.10.0.1".parse().unwrap()));
            assert_eq!(args.port, Some(27100));
            assert!(args.headless);
            assert!(args.dry_run);
            assert!(matches!(args.output_format, CliOutputFormat::Jsonl));
        } else {
            panic!("expected Direct1v1 variant");
        }
    }

    #[test]
    fn direct_tournament_forwards_flags() {
        let cli = Cli::try_parse_from([
            "fragpipe",
            "direct-tournament",
            "--remote",
            "nomad",
            "--max-runs",
            "3",
            "--timeout",
            "600",
            "--no-build",
            "--no-deploy",
            "--local-ip",
            "10.10.0.1",
            "--dry-run",
            "--output-format",
            "jsonl",
        ])
        .unwrap();
        if let Commands::DirectTournament(args) = cli.command {
            assert_eq!(args.remote, "nomad");
            assert_eq!(args.max_runs, Some(3));
            assert_eq!(args.timeout, Some(600));
            assert!(args.no_build);
            assert!(args.no_deploy);
            assert_eq!(args.local_ip, Some("10.10.0.1".parse().unwrap()));
            assert!(args.dry_run);
            assert!(matches!(args.output_format, CliOutputFormat::Jsonl));
        } else {
            panic!("expected DirectTournament variant");
        }
    }

    #[test]
    fn parse_android_1v1_subcommand() {
        let cli = Cli::try_parse_from(["fragpipe", "android-1v1"]).unwrap();
        assert!(matches!(cli.command, Commands::Android1v1(_)));
    }

    #[test]
    fn parse_android_ui_subcommand() {
        let cli = Cli::try_parse_from(["fragpipe", "android-ui"]).unwrap();
        assert!(matches!(cli.command, Commands::AndroidUi(_)));
    }

    #[test]
    fn parse_android_doctor_subcommand() {
        let cli = Cli::try_parse_from(["fragpipe", "android-doctor"]).unwrap();
        assert!(matches!(cli.command, Commands::AndroidDoctor(_)));
    }

    #[test]
    fn android_with_parses_multiple_slots_and_command() {
        let cli = Cli::try_parse_from([
            "fragpipe",
            "android-with",
            "--slot",
            "nomad-aosp35-0",
            "--slot",
            "nomad-aosp35-1",
            "--",
            "scripts/emulator-conformance.sh",
            "full",
        ])
        .unwrap();
        let Commands::AndroidWith(args) = cli.command else {
            panic!("expected AndroidWith variant");
        };
        assert_eq!(args.slot, ["nomad-aosp35-0", "nomad-aosp35-1"]);
        assert_eq!(args.command, ["scripts/emulator-conformance.sh", "full"]);
    }

    #[test]
    fn webrtc_1v1_default_values() {
        let cli = Cli::try_parse_from(["fragpipe", "webrtc-1v1"]).unwrap();
        if let Commands::Webrtc1v1(args) = cli.command {
            assert_eq!(args.config, PathBuf::from("fragpipe.toml"));
            assert!(args.max_runs.is_none());
            assert!(args.timeout.is_none());
            assert!(!args.no_build);
            assert!(!args.no_deploy);
            assert!(args.remote.is_none());
            assert!(args.local_ip.is_none());
            assert!(args.webrtc_port.is_none());
            assert!(!args.dry_run);
            assert!(matches!(args.output_format, CliOutputFormat::Text));
        } else {
            panic!("expected Webrtc1v1 variant");
        }
    }

    #[test]
    fn webrtc_1v1_all_flags() {
        let cli = Cli::try_parse_from([
            "fragpipe",
            "webrtc-1v1",
            "--config",
            "custom.toml",
            "--max-runs",
            "5",
            "--timeout",
            "120",
            "--no-build",
            "--no-deploy",
            "--remote",
            "server1",
            "--local-ip",
            "10.0.0.1",
            "--webrtc-port",
            "8080",
            "--dry-run",
            "--output-format",
            "jsonl",
        ])
        .unwrap();
        if let Commands::Webrtc1v1(args) = cli.command {
            assert_eq!(args.config, PathBuf::from("custom.toml"));
            assert_eq!(args.max_runs, Some(5));
            assert_eq!(args.timeout, Some(120));
            assert!(args.no_build);
            assert!(args.no_deploy);
            assert_eq!(args.remote, Some("server1".into()));
            assert_eq!(args.local_ip, Some("10.0.0.1".parse().unwrap()));
            assert_eq!(args.webrtc_port, Some(8080));
            assert!(args.dry_run);
            assert!(matches!(args.output_format, CliOutputFormat::Jsonl));
        } else {
            panic!("expected Webrtc1v1 variant");
        }
    }

    #[test]
    fn internet_1v1_with_flags() {
        let cli = Cli::try_parse_from([
            "fragpipe",
            "internet-1v1",
            "--workdir",
            "/workspace",
            "--game-bin",
            "game",
            "--rdv-bin",
            "rdv",
            "--dry-run",
        ])
        .unwrap();
        if let Commands::Internet1v1(args) = cli.command {
            assert_eq!(args.workdir, Some(PathBuf::from("/workspace")));
            assert_eq!(args.game_bin, Some(PathBuf::from("game")));
            assert_eq!(args.rdv_bin, Some(PathBuf::from("rdv")));
            assert!(args.dry_run);
        } else {
            panic!("expected Internet1v1 variant");
        }
    }

    #[test]
    fn android_1v1_with_mode_flags() {
        let cli = Cli::try_parse_from([
            "fragpipe",
            "android-1v1",
            "--device",
            "--adb-serial",
            "emulator-5554",
            "--slot",
            "nomad-aosp35-0",
            "--no-install",
            "--dry-run",
        ])
        .unwrap();
        if let Commands::Android1v1(args) = cli.command {
            assert!(args.device);
            assert_eq!(args.adb_serial, Some("emulator-5554".into()));
            assert_eq!(args.slot, Some("nomad-aosp35-0".into()));
            assert!(args.no_install);
            assert!(args.dry_run);
        } else {
            panic!("expected Android1v1 variant");
        }
    }

    #[test]
    fn android_ui_with_launch_config() {
        let cli = Cli::try_parse_from([
            "fragpipe",
            "android-ui",
            "--launch-config",
            r#"{"difficulty":"easy"}"#,
            "--dry-run",
        ])
        .unwrap();
        if let Commands::AndroidUi(args) = cli.command {
            assert_eq!(args.launch_config, Some(r#"{"difficulty":"easy"}"#.into()));
            assert!(args.dry_run);
        } else {
            panic!("expected AndroidUi variant");
        }
    }

    #[test]
    fn parse_ship_subcommand() {
        let cli = Cli::try_parse_from(["fragpipe", "ship", "--remote", "test-peer"]).unwrap();
        assert!(matches!(cli.command, Commands::Ship(_)));
    }

    #[test]
    fn parse_hidden_ssh_gate_subcommand() {
        let cli = Cli::try_parse_from([
            "fragpipe",
            "ssh-gate",
            "--root",
            "/srv/game",
            "--binary",
            "game",
            "--rrsync",
            "/run/current-system/sw/bin/rrsync",
        ])
        .unwrap();
        assert!(matches!(cli.command, Commands::SshGate(_)));
    }

    #[test]
    fn ship_requires_remote() {
        let result = Cli::try_parse_from(["fragpipe", "ship"]);
        assert!(result.is_err());
    }

    #[test]
    fn ship_all_flags() {
        let cli = Cli::try_parse_from([
            "fragpipe",
            "ship",
            "--config",
            "custom.toml",
            "--remote",
            "atlas",
            "--no-build",
            "--no-deploy",
            "--restart",
            "--dry-run",
            "--launch-args",
            "--headless",
            "--auto-play",
        ])
        .unwrap();
        if let Commands::Ship(args) = cli.command {
            assert_eq!(args.config, PathBuf::from("custom.toml"));
            assert_eq!(args.remote, "atlas");
            assert!(args.no_build);
            assert!(args.no_deploy);
            assert!(args.restart);
            assert_eq!(
                args.launch_args,
                Some(vec!["--headless".into(), "--auto-play".into()])
            );
            assert!(args.dry_run);
        } else {
            panic!("expected Ship variant");
        }
    }

    #[test]
    fn ship_default_values() {
        let cli = Cli::try_parse_from(["fragpipe", "ship", "--remote", "nomad"]).unwrap();
        if let Commands::Ship(args) = cli.command {
            assert_eq!(args.config, PathBuf::from("fragpipe.toml"));
            assert_eq!(args.remote, "nomad");
            assert!(!args.no_build);
            assert!(!args.no_deploy);
            assert!(!args.restart);
            assert!(args.launch_args.is_none());
            assert!(!args.dry_run);
        } else {
            panic!("expected Ship variant");
        }
    }

    #[test]
    fn android_doctor_requires_no_extra_args() {
        let cli = Cli::try_parse_from(["fragpipe", "android-doctor"]).unwrap();
        assert!(matches!(cli.command, Commands::AndroidDoctor(_)));
    }

    #[test]
    fn cli_output_format_text_to_output_format() {
        assert_eq!(
            OutputFormat::from(CliOutputFormat::Text),
            OutputFormat::Text
        );
        assert_eq!(
            OutputFormat::from(CliOutputFormat::Jsonl),
            OutputFormat::Jsonl
        );
    }
}
