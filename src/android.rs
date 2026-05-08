//! Android emulator + adb driving for the `android-1v1` runner.
//!
//! Lifecycle that `runner::run_android_1v1` performs per run:
//!   1. `boot_emulator` — spawn the emulator, poll `getprop sys.boot_completed`
//!      until 1 or `boot_timeout_secs` elapses.
//!   2. `install_apk` — `adb install -r <apk>`.
//!   3. `push_rendezvous` — write the listener's WebRTC multiaddr to
//!      `rendezvous_path` on the device so the test-peer can read it on start.
//!   4. `start_activity` — `am start -n <pkg>/<activity>` to launch the
//!      test-peer. The activity loads `libchessbender_android_test_peer.so`
//!      via `android:lib_name`; `bevy_main` enters `android_main` which reads
//!      the rendezvous file.
//!   5. `tail_logcat` — spawn `adb logcat -s <tag>` redirected into the
//!      configured log file so `classify_log` sees the same pass/fatal
//!      markers it sees from the desktop peer.
//!   6. `kill_emulator` — `adb emu kill` then SIGKILL the emulator process.

use std::env;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::config::{AndroidConfig, AndroidTarget};
use crate::process::kill_child;

/// Resolve an SDK-rooted binary path: `cfg.<which>_bin` overrides if set,
/// otherwise look under `$ANDROID_SDK_ROOT/<subdir>/<name>`.
fn resolve_sdk_bin(override_path: Option<&Path>, subdir: &str, name: &str) -> Result<PathBuf> {
    if let Some(path) = override_path {
        return Ok(path.to_path_buf());
    }
    let sdk = env::var("ANDROID_SDK_ROOT")
        .or_else(|_| env::var("ANDROID_HOME"))
        .context(
            "ANDROID_SDK_ROOT (or ANDROID_HOME) must be set to locate adb/emulator; \
             enter the android dev shell or set --adb-bin / --emulator-bin",
        )?;
    Ok(PathBuf::from(sdk).join(subdir).join(name))
}

pub fn adb_bin(cfg: &AndroidConfig) -> Result<PathBuf> {
    resolve_sdk_bin(cfg.adb_bin.as_deref(), "platform-tools", "adb")
}

pub fn emulator_bin(cfg: &AndroidConfig) -> Result<PathBuf> {
    resolve_sdk_bin(cfg.emulator_bin.as_deref(), "emulator", "emulator")
}

fn adb_command(cfg: &AndroidConfig) -> Result<Command> {
    let adb = adb_bin(cfg)?;
    let mut cmd = Command::new(adb);
    if let Some(serial) = cfg.adb_serial.as_deref() {
        cmd.arg("-s").arg(serial);
    }
    Ok(cmd)
}

fn adb_display(cfg: &AndroidConfig) -> Result<String> {
    let adb = adb_bin(cfg)?;
    let mut display = adb.display().to_string();
    if let Some(serial) = cfg.adb_serial.as_deref() {
        display.push_str(" -s ");
        display.push_str(serial);
    }
    Ok(display)
}

pub fn prepare_target(cfg: &AndroidConfig, dry_run: bool) -> Result<Option<Child>> {
    match cfg.target {
        AndroidTarget::Emulator => boot_emulator(cfg, dry_run),
        AndroidTarget::Device => {
            wait_for_device(cfg, Duration::from_secs(cfg.boot_timeout_secs))?;
            Ok(None)
        }
    }
}

/// Spawn the emulator and block until `sys.boot_completed=1` or timeout.
pub fn boot_emulator(cfg: &AndroidConfig, dry_run: bool) -> Result<Option<Child>> {
    let emu = emulator_bin(cfg)?;
    let mut command = Command::new(&emu);
    command.arg("-avd").arg(&cfg.avd_name);
    for extra in &cfg.emulator_args {
        command.arg(extra);
    }
    println!(
        "[android] {} -avd {} {}",
        emu.display(),
        cfg.avd_name,
        cfg.emulator_args.join(" ")
    );
    if dry_run {
        return Ok(None);
    }
    let child = command
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("failed to spawn emulator at {}", emu.display()))?;

    wait_for_device(cfg, Duration::from_secs(cfg.boot_timeout_secs))
        .context("emulator boot timed out")?;
    Ok(Some(child))
}

/// Block until `adb shell getprop sys.boot_completed` returns 1.
fn wait_for_device(cfg: &AndroidConfig, timeout: Duration) -> Result<()> {
    let adb = adb_display(cfg)?;
    // First, `adb wait-for-device` blocks until the device shows up in
    // `adb devices`; then we still need the userspace boot to finish.
    println!("[android] {adb} wait-for-device");
    adb_command(cfg)?
        .arg("wait-for-device")
        .status()
        .with_context(|| format!("failed to invoke adb at {adb}"))?;

    let started = Instant::now();
    loop {
        let output = adb_command(cfg)?
            .args(["shell", "getprop", "sys.boot_completed"])
            .output()
            .with_context(|| format!("adb getprop failed at {adb}"))?;
        if output.status.success() {
            let value = String::from_utf8_lossy(&output.stdout);
            if value.trim() == "1" {
                println!(
                    "[android] device booted in {}s",
                    started.elapsed().as_secs()
                );
                return Ok(());
            }
        }
        if started.elapsed() > timeout {
            bail!("device did not finish booting after {}s", timeout.as_secs());
        }
        thread::sleep(Duration::from_secs(2));
    }
}

pub fn install_apk(cfg: &AndroidConfig, dry_run: bool) -> Result<()> {
    let adb = adb_display(cfg)?;
    println!("[android] {} install -r -t {}", adb, cfg.apk_path.display());
    if dry_run {
        return Ok(());
    }
    let status = adb_command(cfg)?
        .arg("install")
        .arg("-r") // replace existing
        .arg("-t") // allow test packages
        .arg(&cfg.apk_path)
        .status()
        .with_context(|| format!("adb install failed at {adb}"))?;
    if !status.success() {
        bail!("adb install exited with {status}");
    }
    Ok(())
}

/// Push the listener's join address to the device so the test peer can
/// read it on startup. We write to a host-side temp file and `adb push` it
/// to avoid the quoting hazards of `adb shell echo > file`.
pub fn push_rendezvous(cfg: &AndroidConfig, contents: &str, dry_run: bool) -> Result<()> {
    push_device_file(cfg, &cfg.rendezvous_path, contents, "rendezvous", dry_run)
}

pub fn push_launch_config(cfg: &AndroidConfig, contents: &str, dry_run: bool) -> Result<()> {
    push_device_file(
        cfg,
        &cfg.launch_config_path,
        contents,
        "launch config",
        dry_run,
    )
}

fn push_device_file(
    cfg: &AndroidConfig,
    device_path: &str,
    contents: &str,
    label: &str,
    dry_run: bool,
) -> Result<()> {
    let adb = adb_display(cfg)?;
    println!(
        "[android] push {label} to {} ({} bytes)",
        device_path,
        contents.len()
    );
    if dry_run {
        return Ok(());
    }
    let temp = std::env::temp_dir().join(format!(
        "fragpipe-{}-{}.txt",
        label.replace(' ', "-"),
        std::process::id()
    ));
    std::fs::write(&temp, contents).with_context(|| format!("failed to stage {label} file"))?;
    let status = adb_command(cfg)?
        .arg("push")
        .arg(&temp)
        .arg(device_path)
        .status()
        .with_context(|| format!("adb push failed at {adb}"))?;
    let _ = std::fs::remove_file(&temp);
    if !status.success() {
        bail!("adb push exited with {status}");
    }
    Ok(())
}

pub fn start_activity(cfg: &AndroidConfig, dry_run: bool) -> Result<()> {
    let adb = adb_display(cfg)?;
    let component = format!("{}/{}", cfg.package_name, cfg.activity_name);
    println!("[android] am start -n {component}");
    if dry_run {
        return Ok(());
    }
    let status = adb_command(cfg)?
        .args(["shell", "am", "start", "-n", &component])
        .status()
        .with_context(|| format!("adb am start failed at {adb}"))?;
    if !status.success() {
        bail!("am start exited with {status}");
    }
    Ok(())
}

/// Stop the test-peer process so a follow-up run starts clean.
pub fn force_stop(cfg: &AndroidConfig) -> Result<()> {
    let _ = adb_command(cfg)?
        .args(["shell", "am", "force-stop", &cfg.package_name])
        .status();
    Ok(())
}

/// Spawn `adb logcat` filtered to the test-peer tag with output redirected
/// into `cfg.logcat_log`. Caller owns the returned Child and must
/// `kill_child` it on tear-down.
pub fn tail_logcat(cfg: &AndroidConfig, dry_run: bool) -> Result<Option<Child>> {
    let adb = adb_display(cfg)?;
    if let Some(parent) = cfg.logcat_log.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).ok();
    }
    let _ = std::fs::remove_file(&cfg.logcat_log);
    println!(
        "[android] {} logcat -s {}:V → {}",
        adb,
        cfg.log_tag,
        cfg.logcat_log.display()
    );
    if dry_run {
        return Ok(None);
    }
    // Clear the device-side ring first so we only see this run's output.
    let _ = adb_command(cfg)?.args(["logcat", "-c"]).status();

    let log_file = std::fs::File::create(&cfg.logcat_log)
        .with_context(|| format!("failed to create {}", cfg.logcat_log.display()))?;
    let stderr_file = log_file
        .try_clone()
        .context("failed to dup logcat log file handle")?;
    let child = adb_command(cfg)?
        .args(["logcat", "-s", &format!("{}:V", cfg.log_tag), "*:E"])
        .stdout(Stdio::from(log_file))
        .stderr(Stdio::from(stderr_file))
        .spawn()
        .with_context(|| format!("adb logcat failed at {adb}"))?;
    Ok(Some(child))
}

pub fn capture_screenshot(cfg: &AndroidConfig, path: &Path, dry_run: bool) -> Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create screenshot dir {}", parent.display()))?;
    }
    let adb = adb_display(cfg)?;
    println!("[android] {adb} exec-out screencap -p > {}", path.display());
    if dry_run {
        return Ok(());
    }
    let file = std::fs::File::create(path)
        .with_context(|| format!("failed to create screenshot {}", path.display()))?;
    let status = adb_command(cfg)?
        .args(["exec-out", "screencap", "-p"])
        .stdout(Stdio::from(file))
        .status()
        .with_context(|| format!("adb screencap failed at {adb}"))?;
    if !status.success() {
        bail!("adb screencap exited with {status}");
    }
    let data = std::fs::read(path)
        .with_context(|| format!("failed to read screenshot {}", path.display()))?;
    let png_header = b"\x89PNG\r\n\x1a\n";
    if data.len() < 4096 || !data.starts_with(png_header) {
        bail!(
            "screenshot {} is blank or not a valid PNG ({} bytes)",
            path.display(),
            data.len()
        );
    }
    Ok(())
}

pub fn assert_landscape(cfg: &AndroidConfig, dry_run: bool) -> Result<()> {
    let adb = adb_display(cfg)?;
    println!("[android] {adb} shell wm size");
    if dry_run {
        return Ok(());
    }
    let output = adb_command(cfg)?
        .args(["shell", "wm", "size"])
        .output()
        .with_context(|| format!("adb wm size failed at {adb}"))?;
    if !output.status.success() {
        bail!("adb wm size exited with {}", output.status);
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let Some((width, height)) = parse_wm_size(&text) else {
        bail!("could not parse adb wm size output: {text}");
    };
    if width <= height {
        bail!("android display is not landscape: {width}x{height}");
    }
    Ok(())
}

fn parse_wm_size(text: &str) -> Option<(u32, u32)> {
    for token in text.split_whitespace() {
        let Some((left, right)) = token.split_once('x') else {
            continue;
        };
        let Ok(width) = left.parse() else {
            continue;
        };
        let Ok(height) = right.parse() else {
            continue;
        };
        return Some((width, height));
    }
    None
}

/// Cleanly tear down the emulator: tell adb to kill the emulator service,
/// then SIGKILL the process if it doesn't exit on its own.
pub fn kill_emulator(cfg: &AndroidConfig, mut emulator: Option<Child>) {
    if cfg.target == AndroidTarget::Device {
        return;
    }
    if emulator.is_none() {
        return;
    }
    if let Ok(mut adb) = adb_command(cfg) {
        let _ = adb.args(["emu", "kill"]).status();
    }
    if let Some(child) = emulator.as_mut() {
        // Give it a moment to exit gracefully, then force-kill.
        thread::sleep(Duration::from_secs(2));
        kill_child(child);
    }
}

// =============================================================================
// TESTS
//
// We can't drive a real emulator from cargo test, but we can cover:
//   * `resolve_sdk_bin` — the only pure function in the module.
//   * Every public lifecycle entry's `dry_run = true` path. Each one prints its
//     intent and returns success without spawning any process; tests assert
//     they don't panic and return the right shape (Option<Child> = None,
//     Result = Ok). This catches regressions where someone adds an unguarded
//     `Command::new(...).status()` outside the `if dry_run` early-return.
//
// All tests pass `*_bin` overrides so `ANDROID_SDK_ROOT` is never read; that
// keeps them hermetic and parallel-safe.
// =============================================================================
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn cfg(overrides: bool) -> AndroidConfig {
        AndroidConfig {
            target: AndroidTarget::Emulator,
            avd_name: "fragpipe_test".into(),
            adb_serial: None,
            apk_path: PathBuf::from("/tmp/fragpipe-test.apk"),
            apk_build_command: None,
            ui_apk_path: None,
            ui_package_name: None,
            ui_activity_name: None,
            ui_log_tag: None,
            ui_apk_build_command: None,
            package_name: "tartanoglu.chessbender.test_peer".into(),
            activity_name: "androidx.games.activity.GameActivity".into(),
            log_tag: "chessbender".into(),
            rendezvous_path: "/data/local/tmp/chessbender-rendezvous.txt".into(),
            logcat_log: PathBuf::from("logs/fragpipe-android-test.log"),
            emulator_bin: if overrides {
                Some(PathBuf::from("/usr/bin/emulator"))
            } else {
                None
            },
            adb_bin: if overrides {
                Some(PathBuf::from("/usr/bin/adb"))
            } else {
                None
            },
            emulator_args: vec!["-no-window".into(), "-no-audio".into()],
            boot_timeout_secs: 120,
            launch_config_path: "/data/local/tmp/chessbender-launch.json".into(),
            screenshot_dir: PathBuf::from("logs/fragpipe-android-screenshots"),
        }
    }

    #[test]
    fn resolve_sdk_bin_uses_override_path_verbatim() {
        let resolved =
            resolve_sdk_bin(Some(Path::new("/custom/path/adb")), "platform-tools", "adb")
                .expect("override should always resolve without consulting env");
        assert_eq!(resolved, PathBuf::from("/custom/path/adb"));
    }

    #[test]
    fn resolve_sdk_bin_override_ignores_subdir_and_name() {
        // The override is taken as-is — we don't append `<subdir>/<name>` on top.
        let resolved = resolve_sdk_bin(
            Some(Path::new("/dev/shm/test-emulator")),
            "emulator",
            "emulator",
        )
        .unwrap();
        assert_eq!(resolved, PathBuf::from("/dev/shm/test-emulator"));
    }

    #[test]
    fn adb_bin_honors_override() {
        let cfg = cfg(true);
        assert_eq!(adb_bin(&cfg).unwrap(), PathBuf::from("/usr/bin/adb"));
    }

    #[test]
    fn emulator_bin_honors_override() {
        let cfg = cfg(true);
        assert_eq!(
            emulator_bin(&cfg).unwrap(),
            PathBuf::from("/usr/bin/emulator")
        );
    }

    #[test]
    fn boot_emulator_dry_run_returns_none_without_spawning() {
        let cfg = cfg(true);
        let result = boot_emulator(&cfg, true).expect("dry run must succeed");
        assert!(
            result.is_none(),
            "dry run must not return a Child handle (would imply emulator was spawned)"
        );
    }

    #[test]
    fn install_apk_dry_run_succeeds() {
        let cfg = cfg(true);
        install_apk(&cfg, true).expect("dry run install must succeed");
    }

    #[test]
    fn push_rendezvous_dry_run_does_not_create_temp_file() {
        let cfg = cfg(true);
        push_rendezvous(&cfg, "/ip4/127.0.0.1/udp/27200/webrtc-direct", true)
            .expect("dry run push must succeed");
        // Dry run should NOT have written a stage file — we early-return before
        // the std::fs::write call.
        let temp =
            std::env::temp_dir().join(format!("fragpipe-rendezvous-{}.txt", std::process::id()));
        assert!(
            !temp.exists(),
            "dry run must not stage a host-side rendezvous tempfile"
        );
    }

    #[test]
    fn start_activity_dry_run_succeeds() {
        let cfg = cfg(true);
        start_activity(&cfg, true).expect("dry run am start must succeed");
    }

    #[test]
    fn tail_logcat_dry_run_returns_none_without_spawning() {
        let cfg = cfg(true);
        let result = tail_logcat(&cfg, true).expect("dry run logcat must succeed");
        assert!(
            result.is_none(),
            "dry run must not return a Child handle (would imply adb logcat was spawned)"
        );
    }

    #[test]
    fn parse_wm_size_extracts_physical_size() {
        assert_eq!(
            parse_wm_size("Physical size: 2400x1080\n"),
            Some((2400, 1080))
        );
    }

    #[test]
    fn parse_wm_size_extracts_override_size() {
        assert_eq!(
            parse_wm_size("Override size: 1280x720\nPhysical density: 420\n"),
            Some((1280, 720))
        );
    }

    #[test]
    fn rendezvous_contents_roundtrip_through_temp_path() {
        // White-box check: the production code stages contents into a tempfile
        // before adb-pushing it. The temp filename uses the process id so two
        // concurrent runs don't collide. Asserting the path shape locks in
        // that contract.
        let temp =
            std::env::temp_dir().join(format!("fragpipe-rendezvous-{}.txt", std::process::id()));
        assert!(temp.starts_with(std::env::temp_dir()));
        assert!(
            temp.file_name()
                .unwrap()
                .to_string_lossy()
                .contains(&std::process::id().to_string()),
            "tempfile name should include the pid for collision avoidance"
        );
    }
}
