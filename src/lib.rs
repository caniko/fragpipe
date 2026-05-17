pub mod android;
pub mod config;
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

use runner::{
    AndroidDoctorOptions, AndroidRunOptions, AndroidUiRunOptions, OutputFormat, WebRtcRunOptions,
    run_android_1v1, run_android_doctor, run_android_ui, run_webrtc_1v1,
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
    /// Run a native WebRTC Direct 1v1 smoke test.
    #[command(name = "webrtc-1v1")]
    Webrtc1v1(WebRtc1v1Args),
    /// Run a desktop-listener + Android-emulator joiner WebRTC 1v1 smoke test.
    #[command(name = "android-1v1")]
    Android1v1(Android1v1Args),
    /// Launch the Android app, capture a screenshot, and validate landscape UI.
    #[command(name = "android-ui")]
    AndroidUi(AndroidUiArgs),
    /// Validate Android SDK/adb/APK/manifest prerequisites.
    #[command(name = "android-doctor")]
    AndroidDoctor(AndroidDoctorArgs),
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

    /// JSON launch config pushed to the Android device before starting.
    #[arg(long)]
    launch_config: Option<String>,

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
            launch_config: args.launch_config,
            output_format: args.output_format.into(),
        }),
        Commands::AndroidDoctor(args) => run_android_doctor(AndroidDoctorOptions {
            config_path: args.config,
            adb_serial: args.adb_serial,
            device: args.device,
        }),
    }
}
