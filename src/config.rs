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
    pub internet: InternetConfig,
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

#[derive(Debug, Deserialize)]
pub struct InternetConfig {
    /// Chessbender binary passed to the smoke as GAME_BIN. Defaults to [game].binary.
    #[serde(default)]
    pub game_bin: Option<PathBuf>,
    /// Rendezvous/relay binary passed to the smoke as RDV_BIN.
    #[serde(default)]
    pub rdv_bin: Option<PathBuf>,
    /// Asset root passed as ASSET_ROOT. Defaults to <workdir>/assets.
    #[serde(default)]
    pub asset_root: Option<PathBuf>,
    /// Log directory passed as LOG_DIR. Defaults under <workdir>/logs/fragpipe.
    #[serde(default)]
    pub log_dir: Option<PathBuf>,
    /// Overall smoke timeout passed as TIMEOUT_SECS.
    #[serde(default = "default_internet_timeout_secs")]
    pub timeout_secs: u64,
    #[serde(default = "default_max_runs")]
    pub max_runs: u32,
    /// Pass marker passed as PASS_MARKER and used for output classification.
    #[serde(default = "default_internet_pass_marker")]
    pub pass_marker: String,
}

impl Default for InternetConfig {
    fn default() -> Self {
        Self {
            game_bin: None,
            rdv_bin: None,
            asset_root: None,
            log_dir: None,
            timeout_secs: default_internet_timeout_secs(),
            max_runs: default_max_runs(),
            pass_marker: default_internet_pass_marker(),
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
    /// Android target type. `emulator` boots/kills the configured AVD; `device`
    /// uses an already-attached physical device selected by `adb_serial`.
    #[serde(default)]
    pub target: AndroidTarget,
    /// AVD name (must already exist; fragpipe does not create AVDs).
    #[serde(default = "default_android_avd_name")]
    pub avd_name: String,
    /// Optional adb serial. Required when multiple devices are attached and for
    /// deterministic physical-device fix loops.
    #[serde(default)]
    pub adb_serial: Option<String>,
    /// Path to the APK to install on the Android target.
    pub apk_path: PathBuf,
    /// Optional command to build/stage the configured APK before install.
    #[serde(default)]
    pub apk_build_command: Option<String>,
    /// Optional full-app APK path used by `android-ui`.
    #[serde(default)]
    pub ui_apk_path: Option<PathBuf>,
    /// Optional full-app package name used by `android-ui`.
    #[serde(default)]
    pub ui_package_name: Option<String>,
    /// Optional full-app activity name used by `android-ui`.
    #[serde(default)]
    pub ui_activity_name: Option<String>,
    /// Optional full-app logcat tag used by `android-ui`.
    #[serde(default)]
    pub ui_log_tag: Option<String>,
    /// Optional command to build/stage the full-app APK before `android-ui`.
    #[serde(default)]
    pub ui_apk_build_command: Option<String>,
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
    /// Device-side launch config path used by UI-full automation.
    #[serde(default = "default_android_launch_config_path")]
    pub launch_config_path: String,
    /// Host directory for Android UI screenshots and per-run artifacts.
    #[serde(default = "default_android_screenshot_dir")]
    pub screenshot_dir: PathBuf,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AndroidTarget {
    #[default]
    Emulator,
    Device,
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

fn default_internet_timeout_secs() -> u64 {
    200
}

fn default_max_runs() -> u32 {
    1
}

fn default_join_addr_marker() -> String {
    "WEBRTC_JOIN_ADDR=".into()
}

fn default_internet_pass_marker() -> String {
    "GAME OVER".into()
}

fn default_android_activity() -> String {
    "androidx.games.activity.GameActivity".into()
}

fn default_android_avd_name() -> String {
    "Pixel_6_API_34".into()
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

fn default_android_launch_config_path() -> String {
    "/data/local/tmp/chessbender-launch.json".into()
}

fn default_android_screenshot_dir() -> PathBuf {
    PathBuf::from("logs/fragpipe-android-screenshots")
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimal_config_deserializes_with_defaults() {
        let config: Config = toml::from_str(
            r#"
            [game]
            binary = "game"
            "#,
        )
        .unwrap();
        assert_eq!(config.game.name, "game");
        assert!(config.android.is_none());
        assert!(config.remote.is_empty());
    }

    #[test]
    fn all_config_sections_have_sensible_defaults() {
        let config: Config = toml::from_str(
            r#"
            [game]
            binary = "game"
            "#,
        )
        .unwrap();
        assert_eq!(config.webrtc.local_ip, "127.0.0.1".parse::<IpAddr>().unwrap());
        assert_eq!(config.webrtc.port, 27200);
        assert_eq!(config.webrtc.timeout_secs, 300);
        assert_eq!(config.webrtc.max_runs, 1);
        assert_eq!(config.webrtc.join_addr_marker, "WEBRTC_JOIN_ADDR=");
        assert_eq!(config.internet.timeout_secs, 200);
        assert_eq!(config.internet.max_runs, 1);
        assert_eq!(config.internet.pass_marker, "GAME OVER");
        assert_eq!(config.process.pass_markers, vec!["GAME OVER".to_string()]);
        assert!(config.process.fatal_markers.contains(&"panic".to_string()));
        assert!(config.process.fatal_markers.contains(&"[FATAL]".to_string()));
    }

    #[test]
    fn game_config_default_values() {
        let config: Config = toml::from_str(
            r#"
            [game]
            binary = "game"
            "#,
        )
        .unwrap();
        assert_eq!(config.game.name, "game");
        assert_eq!(config.game.listener_log, PathBuf::from("fragpipe-listener.log"));
        assert_eq!(config.game.joiner_log, PathBuf::from("fragpipe-joiner.log"));
        assert!(config.game.build_command.is_none());
        assert!(config.game.env.is_empty());
    }

    #[test]
    fn select_remote_finds_matching_peer() {
        let config: Config = toml::from_str(
            r#"
            [game]
            binary = "game"
            [[remote]]
            name = "peer1"
            host = "host1"
            remote_dir = "/remote"
            [[remote]]
            name = "peer2"
            host = "host2"
            remote_dir = "/remote2"
            "#,
        )
        .unwrap();
        let peer = select_remote(&config, "peer1").unwrap();
        assert_eq!(peer.name, "peer1");
        assert_eq!(peer.host, "host1");
    }

    #[test]
    fn select_remote_errors_on_missing_peer() {
        let config: Config = toml::from_str(
            r#"
            [game]
            binary = "game"
            "#,
        )
        .unwrap();
        assert!(select_remote(&config, "nonexistent").is_err());
    }

    #[test]
    fn project_root_defaults_to_current_dir() {
        let config: Config = toml::from_str(
            r#"
            [game]
            binary = "game"
            "#,
        )
        .unwrap();
        assert_eq!(project_root(&config), Path::new("."));
    }

    #[test]
    fn project_root_uses_configured_path() {
        let config: Config = toml::from_str(
            r#"
            [game]
            binary = "game"
            project_root = "/some/project"
            "#,
        )
        .unwrap();
        assert_eq!(project_root(&config), Path::new("/some/project"));
    }

    #[test]
    fn resolve_path_preserves_absolute_paths() {
        assert_eq!(
            resolve_path(Path::new("/root"), Path::new("/absolute/path")),
            PathBuf::from("/absolute/path")
        );
    }

    #[test]
    fn resolve_path_joins_relative_paths() {
        assert_eq!(
            resolve_path(Path::new("/root"), Path::new("relative/path")),
            PathBuf::from("/root/relative/path")
        );
    }

    #[test]
    fn binary_file_name_extracts_filename() {
        assert_eq!(
            binary_file_name(Path::new("/some/dir/game_bin")).unwrap(),
            "game_bin"
        );
    }

    #[test]
    fn binary_file_name_errors_on_root_path() {
        assert!(binary_file_name(Path::new("/")).is_err());
    }

    #[test]
    fn kill_name_uses_explicit_process_name() {
        let config: Config = toml::from_str(
            r#"
            [game]
            binary = "game"
            [process]
            kill_name = "custom-kill"
            "#,
        )
        .unwrap();
        assert_eq!(kill_name(&config).unwrap(), "custom-kill");
    }

    #[test]
    fn kill_name_falls_back_to_binary_filename() {
        let config: Config = toml::from_str(
            r#"
            [game]
            binary = "/path/to/game_bin"
            "#,
        )
        .unwrap();
        assert_eq!(kill_name(&config).unwrap(), "game_bin");
    }

    #[test]
    fn android_target_emulator_deserializes_from_kebab() {
        let config: Config = toml::from_str(
            r#"
            [game]
            binary = "game"
            [android]
            target = "emulator"
            apk_path = "/apk"
            package_name = "com.test"
            "#,
        )
        .unwrap();
        assert_eq!(
            config.android.as_ref().unwrap().target,
            AndroidTarget::Emulator
        );
    }

    #[test]
    fn android_target_device_deserializes_from_kebab() {
        let config: Config = toml::from_str(
            r#"
            [game]
            binary = "game"
            [android]
            target = "device"
            apk_path = "/apk"
            package_name = "com.test"
            "#,
        )
        .unwrap();
        assert_eq!(
            config.android.as_ref().unwrap().target,
            AndroidTarget::Device
        );
    }

    #[test]
    fn remote_peer_defaults_log_file() {
        let config: Config = toml::from_str(
            r#"
            [game]
            binary = "game"
            [[remote]]
            name = "peer"
            host = "host"
            remote_dir = "/remote"
            "#,
        )
        .unwrap();
        assert_eq!(config.remote[0].log_file, "game.log");
    }

    #[test]
    fn full_config_deserializes_with_android_and_remote() {
        let config: Config = toml::from_str(
            r#"
            [game]
            name = "test-game"
            binary = "bin/test"
            build_command = "cargo build"
            listener_extra_args = ["--headless"]
            joiner_extra_args = ["--connect"]

            [process]
            kill_name = "test"
            pass_markers = ["SUCCESS"]
            fatal_markers = ["CRASH"]

            [webrtc]
            local_ip = "10.0.0.1"
            port = 9090
            timeout_secs = 60
            max_runs = 3

            [internet]
            timeout_secs = 100
            max_runs = 2
            pass_marker = "WIN"

            [android]
            apk_path = "/apk"
            package_name = "com.test"
            avd_name = "Test_AVD"
            adb_serial = "emulator-5554"
            boot_timeout_secs = 120

            [[remote]]
            name = "server"
            host = "10.0.0.2"
            remote_dir = "/app"
            binary_name = "remote-bin"
            "#,
        )
        .unwrap();
        assert_eq!(config.game.name, "test-game");
        assert_eq!(config.webrtc.local_ip, "10.0.0.1".parse::<IpAddr>().unwrap());
        assert_eq!(config.webrtc.port, 9090);
        assert_eq!(config.internet.timeout_secs, 100);
        assert_eq!(config.internet.max_runs, 2);
        assert_eq!(config.internet.pass_marker, "WIN");
        let android = config.android.as_ref().unwrap();
        assert_eq!(android.avd_name, "Test_AVD");
        assert_eq!(android.adb_serial.as_deref(), Some("emulator-5554"));
        assert_eq!(android.boot_timeout_secs, 120);
        assert_eq!(config.remote.len(), 1);
        assert_eq!(config.remote[0].binary_name.as_deref(), Some("remote-bin"));
    }
}
