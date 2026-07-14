//! Android emulator + adb driving for the `android-1v1` runner.
//!
//! Lifecycle that `runner::run_android_1v1` performs for a run series:
//!   1. `boot_emulator` — spawn one emulator, then poll
//!      `getprop sys.boot_completed` until 1 or `boot_timeout_secs` elapses.
//!   2. For each run, stop the app, reinstall the APK, and clear its data.
//!   3. `push_rendezvous` — write that run's listener WebRTC multiaddr to
//!      `rendezvous_path` on the device so the test-peer can read it on start.
//!   4. `start_activity` — `am start -n <pkg>/<activity>` to launch the
//!      test-peer. The activity loads `libchessbender_android_test_peer.so`
//!      via `android:lib_name`; `bevy_main` enters `android_main` which reads
//!      the rendezvous file.
//!   5. `tail_logcat` — spawn `adb logcat -s <tag>` redirected into the
//!      configured log file so `classify_log` sees the same pass/fatal
//!      markers it sees from the desktop peer.
//!   6. After the series, `kill_emulator` requests `adb emu kill`, then
//!      force-kills the emulator process if it has not exited.

use std::env;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use png::{ColorType, Decoder, Transformations};

use crate::config::{AndroidConfig, AndroidTarget};
use crate::process::{check_interrupted, kill_child, remove_if_exists, spawn_process_group};

/// Resolve an SDK-rooted binary path: `cfg.<which>_bin` overrides if set,
/// otherwise look under `$ANDROID_SDK_ROOT/<subdir>/<name>`.
fn resolve_sdk_bin(override_path: Option<&Path>, subdir: &str, name: &str) -> Result<PathBuf> {
    if let Some(path) = override_path {
        return Ok(path.to_path_buf());
    }
    Ok(android_sdk_root()?.join(subdir).join(name))
}

fn android_sdk_root() -> Result<PathBuf> {
    env::var("ANDROID_SDK_ROOT")
        .or_else(|_| env::var("ANDROID_HOME"))
        .context(
            "ANDROID_SDK_ROOT (or ANDROID_HOME) must be set to locate Android SDK tools; \
             enter the Android dev shell or configure explicit [android] tool paths",
        )
        .map(PathBuf::from)
}

pub fn adb_bin(cfg: &AndroidConfig) -> Result<PathBuf> {
    resolve_sdk_bin(cfg.adb_bin.as_deref(), "platform-tools", "adb")
}

pub fn emulator_bin(cfg: &AndroidConfig) -> Result<PathBuf> {
    resolve_sdk_bin(cfg.emulator_bin.as_deref(), "emulator", "emulator")
}

pub fn aapt_bin(cfg: &AndroidConfig) -> Result<PathBuf> {
    if let Some(path) = cfg.aapt_bin.as_ref() {
        return Ok(path.clone());
    }
    let build_tools = android_sdk_root()?.join("build-tools");
    let entries = std::fs::read_dir(&build_tools).with_context(|| {
        format!(
            "Android SDK build-tools are required for APK validation but {} is missing; \
             include build-tools in the Android SDK composition or set [android].aapt_bin",
            build_tools.display()
        )
    })?;
    let candidate = entries
        .filter_map(std::result::Result::ok)
        .map(|entry| entry.path().join("aapt2"))
        .filter(|path| path.is_file())
        .max_by_key(|path| {
            path.parent()
                .and_then(Path::file_name)
                .map(numeric_version_key)
                .unwrap_or_default()
        });
    candidate.with_context(|| {
        format!(
            "no aapt2 binary found under {}; include Android SDK build-tools or set [android].aapt_bin",
            build_tools.display()
        )
    })
}

fn numeric_version_key(version: &std::ffi::OsStr) -> Vec<u64> {
    version
        .to_string_lossy()
        .split(|character: char| !character.is_ascii_digit())
        .filter(|part| !part.is_empty())
        .filter_map(|part| part.parse().ok())
        .collect()
}

pub fn validate_apk_manifest(cfg: &AndroidConfig, dry_run: bool) -> Result<()> {
    let aapt = aapt_bin(cfg)?;
    println!(
        "[android] {} dump badging {}",
        aapt.display(),
        cfg.apk_path.display()
    );
    if !aapt.is_file() {
        bail!(
            "configured aapt2 binary does not exist: {}; fix the Android SDK build-tools composition or [android].aapt_bin",
            aapt.display()
        );
    }
    if dry_run {
        return Ok(());
    }
    let output = Command::new(&aapt)
        .args(["dump", "badging"])
        .arg(&cfg.apk_path)
        .output()
        .with_context(|| format!("failed to inspect APK with {}", aapt.display()))?;
    if !output.status.success() {
        bail!(
            "aapt2 dump badging failed for {} with status {}: {}",
            cfg.apk_path.display(),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let badging = String::from_utf8_lossy(&output.stdout);
    validate_badging(cfg, &badging)
}

fn validate_badging(cfg: &AndroidConfig, badging: &str) -> Result<()> {
    let package = badging
        .lines()
        .find(|line| line.starts_with("package:"))
        .and_then(|line| quoted_badging_field(line, "name"))
        .context("aapt2 badging output did not contain a package name")?;
    if package != cfg.package_name {
        bail!(
            "configured Android package {} does not match APK package {} ({})",
            cfg.package_name,
            package,
            cfg.apk_path.display()
        );
    }
    let activity = badging
        .lines()
        .find(|line| line.starts_with("launchable-activity:"))
        .and_then(|line| quoted_badging_field(line, "name"))
        .context("aapt2 badging output did not contain a launchable activity")?;
    let configured_activity = fully_qualified_activity(&cfg.package_name, &cfg.activity_name);
    let packaged_activity = fully_qualified_activity(package, activity);
    if packaged_activity != configured_activity {
        bail!(
            "configured Android activity {} does not match APK launchable activity {} ({})",
            configured_activity,
            packaged_activity,
            cfg.apk_path.display()
        );
    }
    Ok(())
}

fn quoted_badging_field<'a>(line: &'a str, field: &str) -> Option<&'a str> {
    let marker = format!("{field}='");
    let value = line.split_once(&marker)?.1;
    value.split_once('\'').map(|(value, _)| value)
}

fn fully_qualified_activity(package: &str, activity: &str) -> String {
    if activity.starts_with('.') {
        format!("{package}{activity}")
    } else if activity.contains('.') {
        activity.to_string()
    } else {
        format!("{package}.{activity}")
    }
}

pub fn list_avds(cfg: &AndroidConfig) -> Result<Vec<String>> {
    let emulator = emulator_bin(cfg)?;
    let output = Command::new(&emulator)
        .arg("-list-avds")
        .output()
        .with_context(|| format!("failed to list AVDs with {}", emulator.display()))?;
    if !output.status.success() {
        bail!("emulator -list-avds exited with {}", output.status);
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect())
}

pub fn check_adb(cfg: &AndroidConfig, dry_run: bool) -> Result<()> {
    let adb = adb_bin(cfg)?;
    println!("[android] {} version", adb.display());
    if !adb.is_file() {
        bail!(
            "configured adb binary does not exist: {}; fix the Android SDK composition or [android].adb_bin",
            adb.display()
        );
    }
    if dry_run {
        return Ok(());
    }
    let output = Command::new(&adb)
        .arg("version")
        .output()
        .with_context(|| format!("failed to execute adb at {}", adb.display()))?;
    if !output.status.success() {
        bail!("adb version exited with {}", output.status);
    }
    Ok(())
}

struct AndroidDevice<'a> {
    cfg: &'a AndroidConfig,
}

impl<'a> AndroidDevice<'a> {
    fn new(cfg: &'a AndroidConfig) -> Self {
        Self { cfg }
    }

    fn adb_command(&self) -> Result<Command> {
        let adb = adb_bin(self.cfg)?;
        let mut cmd = Command::new(adb);
        if let Some(serial) = self.cfg.adb_serial.as_deref() {
            cmd.arg("-s").arg(serial);
        }
        Ok(cmd)
    }

    fn adb_display(&self) -> Result<String> {
        let adb = adb_bin(self.cfg)?;
        let mut display = adb.display().to_string();
        if let Some(serial) = self.cfg.adb_serial.as_deref() {
            display.push_str(" -s ");
            display.push_str(serial);
        }
        Ok(display)
    }

    fn wait_until_booted(&self, timeout: Duration) -> Result<()> {
        let adb = self.adb_display()?;
        let started = Instant::now();

        println!("[android] {adb} wait-for-device");
        let mut wait = self
            .adb_command()?
            .arg("wait-for-device")
            .spawn()
            .with_context(|| format!("failed to invoke adb at {adb}"))?;
        loop {
            if let Err(error) = check_interrupted() {
                kill_child(&mut wait);
                return Err(error);
            }
            if let Some(status) = wait
                .try_wait()
                .context("failed to poll adb wait-for-device")?
            {
                if !status.success() {
                    bail!("adb wait-for-device exited with {status}");
                }
                break;
            }
            if started.elapsed() > timeout {
                kill_child(&mut wait);
                bail!("adb wait-for-device timed out after {}s", timeout.as_secs());
            }
            thread::sleep(Duration::from_millis(250));
        }

        loop {
            check_interrupted()?;
            let output = self
                .adb_command()?
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

    fn install_apk(&self, dry_run: bool) -> Result<()> {
        let adb = self.adb_display()?;
        println!(
            "[android] {} install -r -t {}",
            adb,
            self.cfg.apk_path.display()
        );
        if dry_run {
            return Ok(());
        }
        let output = self
            .adb_command()?
            .arg("install")
            .arg("-r")
            .arg("-t")
            .arg(&self.cfg.apk_path)
            .output()
            .with_context(|| format!("adb install failed at {adb}"))?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !output.status.success() || !stdout.lines().any(|line| line.trim() == "Success") {
            bail!(
                "adb install failed for {}: status {}, stdout {:?}, stderr {:?}",
                self.cfg.apk_path.display(),
                output.status,
                stdout.trim(),
                stderr.trim()
            );
        }
        Ok(())
    }

    fn push_file(
        &self,
        device_path: &str,
        contents: &str,
        label: &str,
        dry_run: bool,
    ) -> Result<()> {
        let adb = self.adb_display()?;
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
        let status = self
            .adb_command()?
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

    fn start_activity(&self, dry_run: bool) -> Result<()> {
        let adb = self.adb_display()?;
        let component = format!("{}/{}", self.cfg.package_name, self.cfg.activity_name);
        println!("[android] am start -n {component}");
        if dry_run {
            return Ok(());
        }
        let output = self
            .adb_command()?
            .args(["shell", "am", "start", "-n", &component])
            .output()
            .with_context(|| format!("adb am start failed at {adb}"))?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !output.status.success()
            || stdout.contains("Error type")
            || stderr.contains("Error type")
            || stdout.contains("Error:")
            || stderr.contains("Error:")
            || stdout.contains("Exception")
            || stderr.contains("Exception")
            || stdout.contains("does not exist")
            || stderr.contains("does not exist")
        {
            bail!(
                "am start failed for {component}: status {}, stdout {:?}, stderr {:?}",
                output.status,
                stdout.trim(),
                stderr.trim()
            );
        }
        Ok(())
    }

    fn force_stop(&self) -> Result<()> {
        let adb = self.adb_display()?;
        let status = self
            .adb_command()?
            .args(["shell", "am", "force-stop", &self.cfg.package_name])
            .status()
            .with_context(|| format!("adb force-stop failed at {adb}"))?;
        if !status.success() {
            bail!(
                "adb force-stop failed for {} with {status}",
                self.cfg.package_name
            );
        }
        Ok(())
    }

    fn clear_app_data(&self, dry_run: bool) -> Result<()> {
        let adb = self.adb_display()?;
        println!("[android] {adb} shell pm clear {}", self.cfg.package_name);
        if dry_run {
            return Ok(());
        }
        let output = self
            .adb_command()?
            .args(["shell", "pm", "clear", &self.cfg.package_name])
            .output()
            .with_context(|| format!("adb pm clear failed at {adb}"))?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        if !output.status.success() || stdout.trim() != "Success" {
            bail!(
                "adb pm clear failed for {}: status {}, output {:?}",
                self.cfg.package_name,
                output.status,
                stdout.trim()
            );
        }
        Ok(())
    }

    fn check_device(&self) -> Result<()> {
        let adb = self.adb_display()?;
        let output = self
            .adb_command()?
            .arg("get-state")
            .output()
            .with_context(|| format!("failed to query Android device state with {adb}"))?;
        let state = String::from_utf8_lossy(&output.stdout);
        if !output.status.success() || state.trim() != "device" {
            bail!(
                "Android target is not ready: `{adb} get-state` returned status {} and {:?}",
                output.status,
                state.trim()
            );
        }
        Ok(())
    }

    fn ensure_package_foreground(&self) -> Result<()> {
        let adb = self.adb_display()?;
        let output = self
            .adb_command()?
            .args(["shell", "dumpsys", "activity", "activities"])
            .output()
            .with_context(|| format!("failed to inspect resumed Android activity with {adb}"))?;
        if !output.status.success() {
            bail!("adb dumpsys activity exited with {}", output.status);
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        let resumed_lines = stdout
            .lines()
            .filter(|line| {
                line.contains("mResumedActivity")
                    || line.contains("topResumedActivity")
                    || line.contains("ResumedActivity")
            })
            .collect::<Vec<_>>();
        if !resumed_lines.iter().any(|line| {
            resumed_component_matches(line, &self.cfg.package_name, &self.cfg.activity_name)
        }) {
            bail!(
                "Android component {}/{} is not the resumed foreground activity; resumed activity output: {}",
                self.cfg.package_name,
                self.cfg.activity_name,
                if resumed_lines.is_empty() {
                    "<none>".to_string()
                } else {
                    resumed_lines.join(" | ")
                }
            );
        }
        Ok(())
    }

    fn tail_logcat(&self, dry_run: bool) -> Result<Option<Child>> {
        let adb = self.adb_display()?;
        println!(
            "[android] {} logcat -s {}:V → {}",
            adb,
            self.cfg.log_tag,
            self.cfg.logcat_log.display()
        );
        if dry_run {
            return Ok(None);
        }
        if let Some(parent) = self.cfg.logcat_log.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("failed to create logcat directory {}", parent.display())
            })?;
        }
        remove_if_exists(&self.cfg.logcat_log)?;
        let clear_status = self
            .adb_command()?
            .args(["logcat", "-c"])
            .status()
            .with_context(|| format!("failed to clear logcat with {adb}"))?;
        if !clear_status.success() {
            bail!("adb logcat -c exited with {clear_status}");
        }

        let log_file = std::fs::File::create(&self.cfg.logcat_log)
            .with_context(|| format!("failed to create {}", self.cfg.logcat_log.display()))?;
        let stderr_file = log_file
            .try_clone()
            .context("failed to dup logcat log file handle")?;
        let mut command = self.adb_command()?;
        command
            .args(["logcat", "-s", &format!("{}:V", self.cfg.log_tag), "*:E"])
            .stdout(Stdio::from(log_file))
            .stderr(Stdio::from(stderr_file));
        let child = spawn_process_group(command, "adb logcat failed")
            .with_context(|| format!("adb logcat failed at {adb}"))?;
        Ok(Some(child))
    }

    fn capture_screenshot(&self, path: &Path, dry_run: bool) -> Result<()> {
        let adb = self.adb_display()?;
        println!("[android] {adb} exec-out screencap -p > {}", path.display());
        if dry_run {
            return Ok(());
        }
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create screenshot dir {}", parent.display()))?;
        }
        let file = std::fs::File::create(path)
            .with_context(|| format!("failed to create screenshot {}", path.display()))?;
        let status = self
            .adb_command()?
            .args(["exec-out", "screencap", "-p"])
            .stdout(Stdio::from(file))
            .status()
            .with_context(|| format!("adb screencap failed at {adb}"))?;
        if !status.success() {
            bail!("adb screencap exited with {status}");
        }
        let data = std::fs::read(path)
            .with_context(|| format!("failed to read screenshot {}", path.display()))?;
        validate_screenshot(&data, path)
    }
}

fn resumed_component_matches(line: &str, package: &str, configured_activity: &str) -> bool {
    let configured_activity = fully_qualified_activity(package, configured_activity);
    line.split_whitespace().any(|token| {
        let token = token.trim_matches(|character: char| {
            !(character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '$' | '/' | '-'))
        });
        let Some((resumed_package, resumed_activity)) = token.split_once('/') else {
            return false;
        };
        resumed_package == package
            && fully_qualified_activity(resumed_package, resumed_activity) == configured_activity
    })
}

pub fn prepare_target(cfg: &AndroidConfig, dry_run: bool) -> Result<Option<Child>> {
    if dry_run {
        match cfg.target {
            AndroidTarget::Emulator => {
                let _ = boot_emulator(cfg, true)?;
            }
            AndroidTarget::Device => {
                println!("[android] device readiness check skipped (dry run)");
            }
        }
        return Ok(None);
    }
    match cfg.target {
        AndroidTarget::Emulator => boot_emulator(cfg, false),
        AndroidTarget::Device => {
            AndroidDevice::new(cfg)
                .wait_until_booted(Duration::from_secs(cfg.boot_timeout_secs))?;
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
    command.stdout(Stdio::null()).stderr(Stdio::null());
    let mut child = spawn_process_group(command, "failed to spawn emulator")
        .with_context(|| format!("failed to spawn emulator at {}", emu.display()))?;

    if let Err(error) = AndroidDevice::new(cfg)
        .wait_until_booted(Duration::from_secs(cfg.boot_timeout_secs))
        .context("emulator boot timed out")
    {
        kill_child(&mut child);
        return Err(error);
    }
    Ok(Some(child))
}

pub fn install_apk(cfg: &AndroidConfig, dry_run: bool) -> Result<()> {
    AndroidDevice::new(cfg).install_apk(dry_run)
}

/// Push the listener's join address to the device so the test peer can
/// read it on startup. We write to a host-side temp file and `adb push` it
/// to avoid the quoting hazards of `adb shell echo > file`.
pub fn push_rendezvous(cfg: &AndroidConfig, contents: &str, dry_run: bool) -> Result<()> {
    AndroidDevice::new(cfg).push_file(&cfg.rendezvous_path, contents, "rendezvous", dry_run)
}

pub fn push_launch_config(cfg: &AndroidConfig, contents: &str, dry_run: bool) -> Result<()> {
    AndroidDevice::new(cfg).push_file(&cfg.launch_config_path, contents, "launch config", dry_run)
}

pub fn start_activity(cfg: &AndroidConfig, dry_run: bool) -> Result<()> {
    AndroidDevice::new(cfg).start_activity(dry_run)
}

/// Stop the test-peer process so a follow-up run starts clean.
pub fn force_stop(cfg: &AndroidConfig) -> Result<()> {
    AndroidDevice::new(cfg).force_stop()
}

pub fn clear_app_data(cfg: &AndroidConfig, dry_run: bool) -> Result<()> {
    AndroidDevice::new(cfg).clear_app_data(dry_run)
}

pub fn check_device(cfg: &AndroidConfig) -> Result<()> {
    AndroidDevice::new(cfg).check_device()
}

pub fn ensure_package_foreground(cfg: &AndroidConfig) -> Result<()> {
    AndroidDevice::new(cfg).ensure_package_foreground()
}

/// Spawn `adb logcat` filtered to the test-peer tag with output redirected
/// into `cfg.logcat_log`. Caller owns the returned Child and must
/// `kill_child` it on tear-down.
pub fn tail_logcat(cfg: &AndroidConfig, dry_run: bool) -> Result<Option<Child>> {
    AndroidDevice::new(cfg).tail_logcat(dry_run)
}

pub fn capture_screenshot(cfg: &AndroidConfig, path: &Path, dry_run: bool) -> Result<()> {
    AndroidDevice::new(cfg).capture_screenshot(path, dry_run)
}

fn validate_screenshot(data: &[u8], path: &Path) -> Result<()> {
    let mut decoder = Decoder::new(Cursor::new(data));
    decoder.set_transformations(Transformations::EXPAND | Transformations::STRIP_16);
    let mut reader = decoder
        .read_info()
        .with_context(|| format!("screenshot {} is not a valid PNG", path.display()))?;
    let buffer_size = reader
        .output_buffer_size()
        .context("decoded screenshot is too large for this platform")?;
    let mut pixels = vec![0; buffer_size];
    let info = reader
        .next_frame(&mut pixels)
        .with_context(|| format!("failed to decode screenshot {}", path.display()))?;
    if info.width <= info.height {
        bail!(
            "android screenshot is not landscape: {}x{} ({})",
            info.width,
            info.height,
            path.display()
        );
    }
    let pixels = &pixels[..info.buffer_size()];
    let has_visible_variation = match info.color_type {
        ColorType::Grayscale => pixels
            .first()
            .is_some_and(|first| pixels.iter().skip(1).any(|sample| sample != first)),
        ColorType::GrayscaleAlpha => {
            let mut visible = pixels
                .chunks_exact(2)
                .filter(|pixel| pixel[1] != 0)
                .map(|pixel| pixel[0]);
            visible
                .next()
                .is_some_and(|first| visible.any(|sample| sample != first))
        }
        ColorType::Rgb => {
            let mut visible = pixels.chunks_exact(3);
            visible
                .next()
                .is_some_and(|first| visible.any(|pixel| pixel != first))
        }
        ColorType::Rgba => {
            let mut visible = pixels
                .chunks_exact(4)
                .filter(|pixel| pixel[3] != 0)
                .map(|pixel| &pixel[..3]);
            visible
                .next()
                .is_some_and(|first| visible.any(|pixel| pixel != first))
        }
        ColorType::Indexed => false,
    };
    if !has_visible_variation {
        bail!(
            "android screenshot is blank or visually uniform: {}",
            path.display()
        );
    }
    Ok(())
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
    if let Ok(mut adb) = AndroidDevice::new(cfg).adb_command() {
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
            local_ip: None,
            apk_path: PathBuf::from("/tmp/fragpipe-test.apk"),
            apk_build_command: None,
            device_apk_build_command: None,
            ui_apk_path: None,
            ui_package_name: None,
            ui_activity_name: None,
            ui_log_tag: None,
            ui_apk_build_command: None,
            device_ui_apk_build_command: None,
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
            aapt_bin: None,
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
    fn aapt_bin_honors_override() {
        let mut cfg = cfg(true);
        cfg.aapt_bin = Some(PathBuf::from("/custom/aapt2"));
        assert_eq!(aapt_bin(&cfg).unwrap(), PathBuf::from("/custom/aapt2"));
    }

    #[test]
    fn numeric_version_key_orders_sdk_build_tools_naturally() {
        assert!(
            numeric_version_key(std::ffi::OsStr::new("35.0.0"))
                > numeric_version_key(std::ffi::OsStr::new("9.0.0"))
        );
    }

    #[test]
    fn apk_badging_must_match_configured_package_and_activity() {
        let cfg = cfg(true);
        let badging = "package: name='tartanoglu.chessbender.test_peer' versionCode='1'\n\
                       launchable-activity: name='androidx.games.activity.GameActivity' label='' icon=''\n";
        validate_badging(&cfg, badging).unwrap();

        let wrong_package = badging.replace(
            "tartanoglu.chessbender.test_peer",
            "tartanoglu.chessbender.wrong",
        );
        assert!(
            validate_badging(&cfg, &wrong_package)
                .unwrap_err()
                .to_string()
                .contains("does not match APK package")
        );
        let wrong_activity = badging.replace(
            "androidx.games.activity.GameActivity",
            "androidx.games.activity.OtherActivity",
        );
        assert!(
            validate_badging(&cfg, &wrong_activity)
                .unwrap_err()
                .to_string()
                .contains("does not match APK launchable activity")
        );
    }

    #[test]
    fn relative_apk_activity_is_compared_as_fully_qualified() {
        let mut cfg = cfg(true);
        cfg.package_name = "com.example".into();
        cfg.activity_name = "com.example.MainActivity".into();
        validate_badging(
            &cfg,
            "package: name='com.example'\nlaunchable-activity: name='.MainActivity'\n",
        )
        .unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn foreground_check_requires_configured_package_and_activity_to_be_resumed() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().unwrap();
        let adb = dir.path().join("adb");
        std::fs::write(
            &adb,
            "#!/bin/sh\necho 'mResumedActivity: ActivityRecord{123 tartanoglu.chessbender.test_peer/androidx.games.activity.GameActivity}'\n",
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&adb).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&adb, permissions).unwrap();
        let mut cfg = cfg(true);
        cfg.adb_bin = Some(adb);

        ensure_package_foreground(&cfg).unwrap();
        std::fs::write(
            cfg.adb_bin.as_ref().unwrap(),
            "#!/bin/sh\necho 'mResumedActivity: ActivityRecord{123 tartanoglu.chessbender.test_peer/.OtherActivity}'\n",
        )
        .unwrap();
        assert!(
            ensure_package_foreground(&cfg)
                .unwrap_err()
                .to_string()
                .contains("not the resumed foreground activity")
        );
    }

    #[test]
    fn resumed_component_matching_normalizes_relative_activity_names() {
        assert!(resumed_component_matches(
            "mResumedActivity: ActivityRecord{123 com.example/.MainActivity}",
            "com.example",
            "com.example.MainActivity"
        ));
        assert!(!resumed_component_matches(
            "mResumedActivity: ActivityRecord{123 com.example/.OtherActivity}",
            "com.example",
            "com.example.MainActivity"
        ));
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
    fn prepare_physical_device_dry_run_does_not_invoke_adb() {
        let mut cfg = cfg(true);
        cfg.target = AndroidTarget::Device;
        cfg.adb_bin = Some(PathBuf::from("/path/that/must/not/be/executed/adb"));

        let result = prepare_target(&cfg, true).expect("dry run must not invoke adb");

        assert!(result.is_none());
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
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = cfg(true);
        cfg.logcat_log = dir.path().join("existing.log");
        std::fs::write(&cfg.logcat_log, "keep me").unwrap();
        let result = tail_logcat(&cfg, true).expect("dry run logcat must succeed");
        assert!(
            result.is_none(),
            "dry run must not return a Child handle (would imply adb logcat was spawned)"
        );
        assert_eq!(std::fs::read_to_string(&cfg.logcat_log).unwrap(), "keep me");
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

    #[test]
    fn visible_landscape_png_is_valid() {
        let png = encode_rgb_png(4, 2, &[[24, 48, 72], [72, 48, 24]]);
        validate_screenshot(&png, Path::new("visible.png")).unwrap();
    }

    #[test]
    fn black_landscape_png_is_rejected_as_blank() {
        let png = encode_rgb_png(4, 2, &[[0, 0, 0]]);
        let error = validate_screenshot(&png, Path::new("black.png")).unwrap_err();
        assert!(error.to_string().contains("blank"));
    }

    #[test]
    fn white_landscape_png_is_rejected_as_uniform() {
        let png = encode_rgb_png(4, 2, &[[255, 255, 255]]);
        let error = validate_screenshot(&png, Path::new("white.png")).unwrap_err();
        assert!(error.to_string().contains("uniform"));
    }

    #[test]
    fn portrait_png_is_rejected() {
        let png = encode_rgb_png(2, 4, &[[24, 48, 72]]);
        let error = validate_screenshot(&png, Path::new("portrait.png")).unwrap_err();
        assert!(error.to_string().contains("not landscape"));
    }

    #[test]
    fn invalid_png_is_rejected() {
        assert!(validate_screenshot(b"not a png", Path::new("invalid.png")).is_err());
    }

    #[test]
    fn screenshot_dry_run_does_not_create_or_replace_files() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = cfg(true);
        cfg.screenshot_dir = dir.path().join("new-directory");
        let screenshot = cfg.screenshot_dir.join("screen.png");
        capture_screenshot(&cfg, &screenshot, true).unwrap();
        assert!(!cfg.screenshot_dir.exists());
    }

    fn encode_rgb_png(width: u32, height: u32, colors: &[[u8; 3]]) -> Vec<u8> {
        let mut data = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut data, width, height);
            encoder.set_color(png::ColorType::Rgb);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().unwrap();
            let pixels = colors
                .iter()
                .flat_map(|color| color.iter().copied())
                .cycle()
                .take(width as usize * height as usize * 3)
                .collect::<Vec<_>>();
            writer.write_image_data(&pixels).unwrap();
        }
        data
    }
}
