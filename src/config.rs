use std::ffi::OsStr;
use std::fs;
use std::net::IpAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub game: GameConfig,
    #[serde(default)]
    pub process: ProcessConfig,
    #[serde(default)]
    pub webrtc: WebRtcConfig,
    #[serde(default)]
    pub android: Option<AndroidConfig>,
    #[serde(default)]
    pub remote: Vec<RemotePeer>,
    #[serde(default)]
    #[serde(rename = "steampipe_command")]
    pub _steampipe_command: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct GameConfig {
    #[serde(default = "default_game_name")]
    pub name: String,
    pub binary: PathBuf,
    #[serde(default)]
    pub build_command: Option<String>,
    #[serde(default)]
    pub project_root: Option<PathBuf>,
    #[serde(default)]
    pub assets_dir: Option<PathBuf>,
    #[serde(default = "default_listener_log", alias = "local_log")]
    pub listener_log: PathBuf,
    #[serde(default = "default_joiner_log")]
    pub joiner_log: PathBuf,
    #[serde(default)]
    pub env: Vec<EnvPair>,
    #[serde(default, alias = "host_args")]
    pub listener_extra_args: Vec<String>,
    #[serde(default, alias = "join_args")]
    pub joiner_extra_args: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EnvPair {
    pub name: String,
    pub value: String,
}

#[derive(Debug, Deserialize)]
pub struct ProcessConfig {
    #[serde(default)]
    pub kill_name: Option<String>,
    #[serde(default = "default_pass_markers")]
    pub pass_markers: Vec<String>,
    #[serde(default = "default_fatal_markers")]
    pub fatal_markers: Vec<String>,
}

impl Default for ProcessConfig {
    fn default() -> Self {
        Self {
            kill_name: None,
            pass_markers: default_pass_markers(),
            fatal_markers: default_fatal_markers(),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct WebRtcConfig {
    #[serde(default = "default_local_ip")]
    pub local_ip: IpAddr,
    #[serde(default = "default_webrtc_port")]
    pub port: u16,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    #[serde(default = "default_max_runs")]
    pub max_runs: u32,
    #[serde(default = "default_join_addr_marker")]
    pub join_addr_marker: String,
    #[serde(default)]
    pub listener_args: Vec<String>,
    #[serde(default)]
    pub joiner_args: Vec<String>,
}

impl Default for WebRtcConfig {
    fn default() -> Self {
        Self {
            local_ip: default_local_ip(),
            port: default_webrtc_port(),
            timeout_secs: default_timeout_secs(),
            max_runs: default_max_runs(),
            join_addr_marker: default_join_addr_marker(),
            listener_args: Vec::new(),
            joiner_args: Vec::new(),
        }
    }
}

/// Android emulator + APK driving for the `android-1v1` runner.
///
/// Fragpipe boots a pre-baked AVD, installs the test-peer APK, pushes a
/// rendezvous file with the listener's WebRTC multiaddr, fires the activity
/// via `am start`, and tails logcat looking for the same pass/fatal markers
/// the desktop peer emits.
#[derive(Debug, Deserialize, Clone)]
pub struct AndroidConfig {
    /// AVD name (must already exist; fragpipe does not create AVDs).
    pub avd_name: String,
    /// Path to the test-peer APK to install on the emulator.
    pub apk_path: PathBuf,
    /// Application package name (e.g. `tartanoglu.chessbender.test_peer`).
    pub package_name: String,
    /// Fully-qualified activity name (e.g. `androidx.games.activity.GameActivity`).
    #[serde(default = "default_android_activity")]
    pub activity_name: String,
    /// Logcat tag the test-peer logs under (defaults to `chessbender`).
    #[serde(default = "default_android_log_tag")]
    pub log_tag: String,
    /// Path on the emulator where the listener's join address gets pushed.
    #[serde(default = "default_android_rendezvous_path")]
    pub rendezvous_path: String,
    /// Path on host for the captured logcat stream — fed to the existing
    /// classify_log marker engine the same way `joiner_log` is on desktop.
    #[serde(default = "default_android_log")]
    pub logcat_log: PathBuf,
    /// Optional `emulator` binary override (defaults to `$ANDROID_SDK_ROOT/emulator/emulator`).
    #[serde(default)]
    pub emulator_bin: Option<PathBuf>,
    /// Optional `adb` binary override (defaults to `$ANDROID_SDK_ROOT/platform-tools/adb`).
    #[serde(default)]
    pub adb_bin: Option<PathBuf>,
    /// Extra args to pass to the emulator process (e.g. `["-no-window", "-no-audio"]`).
    #[serde(default = "default_android_emulator_args")]
    pub emulator_args: Vec<String>,
    /// Seconds to wait for the emulator to reach `sys.boot_completed=1`.
    #[serde(default = "default_android_boot_timeout_secs")]
    pub boot_timeout_secs: u64,
}

#[derive(Debug, Deserialize)]
pub struct RemotePeer {
    pub name: String,
    pub host: String,
    pub remote_dir: String,
    #[serde(default = "default_remote_log")]
    pub log_file: String,
    #[serde(default)]
    pub binary_name: Option<String>,
    #[serde(default)]
    pub assets_dir: Option<PathBuf>,
    #[serde(default)]
    pub deploy: Vec<DeployPath>,
    #[serde(default)]
    pub env: Vec<EnvPair>,
    #[serde(default)]
    pub join_args: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct DeployPath {
    pub source: PathBuf,
    pub target: String,
}

pub fn load_config(path: &Path) -> Result<Config> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("failed to read config {}", path.display()))?;
    toml::from_str(&text).with_context(|| format!("failed to parse config {}", path.display()))
}

pub fn select_remote<'a>(config: &'a Config, name: &str) -> Result<&'a RemotePeer> {
    config
        .remote
        .iter()
        .find(|remote| remote.name == name)
        .ok_or_else(|| anyhow!("remote peer '{name}' is not defined"))
}

pub fn project_root(config: &Config) -> &Path {
    config
        .game
        .project_root
        .as_deref()
        .unwrap_or_else(|| Path::new("."))
}

pub fn resolve_path(root: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    }
}

pub fn binary_file_name(path: &Path) -> Result<String> {
    path.file_name()
        .and_then(OsStr::to_str)
        .map(str::to_string)
        .ok_or_else(|| anyhow!("game.binary must have a file name"))
}

pub fn kill_name(config: &Config) -> Result<String> {
    match config.process.kill_name.as_ref() {
        Some(name) => Ok(name.clone()),
        None => binary_file_name(&config.game.binary),
    }
}

fn default_game_name() -> String {
    "game".into()
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

fn default_join_addr_marker() -> String {
    "WEBRTC_JOIN_ADDR=".into()
}

fn default_android_activity() -> String {
    "androidx.games.activity.GameActivity".into()
}

fn default_android_log_tag() -> String {
    "chessbender".into()
}

fn default_android_rendezvous_path() -> String {
    "/data/local/tmp/chessbender-rendezvous.txt".into()
}

fn default_android_log() -> PathBuf {
    PathBuf::from("fragpipe-android.log")
}

fn default_android_emulator_args() -> Vec<String> {
    vec![
        "-no-window".into(),
        "-no-audio".into(),
        "-no-snapshot-save".into(),
        "-gpu".into(),
        "swiftshader_indirect".into(),
    ]
}

fn default_android_boot_timeout_secs() -> u64 {
    180
}

fn default_pass_markers() -> Vec<String> {
    vec!["GAME OVER".into()]
}

fn default_fatal_markers() -> Vec<String> {
    vec![
        "[FATAL]".into(),
        "panic".into(),
        "DAG_VIOLATION".into(),
        "DESYNC".into(),
        "CONNECTION_LOST".into(),
        "PEER_READY_TIMEOUT".into(),
        "Graceful shutdown: exit_code=".into(),
    ]
}
