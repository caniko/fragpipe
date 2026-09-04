use std::env;
use std::fs;
use std::io::{self, BufRead, BufReader};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use anyhow::{Context, Result, bail};

use crate::config::{
    AndroidConfig, AndroidWorker, AndroidWorkerSlot, load_worker_config, select_android_slots,
};
use crate::process::{ensure_success, kill_child};
use crate::util::{shell_args, shell_quote};

#[derive(Debug)]
pub struct AndroidCommandError(std::process::ExitStatus);

impl AndroidCommandError {
    pub fn exit_code(&self) -> i32 {
        self.0.code().unwrap_or(1)
    }
}

impl std::fmt::Display for AndroidCommandError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "Android worker command failed with status {}",
            self.0
        )
    }
}

impl std::error::Error for AndroidCommandError {}

pub struct AndroidLease {
    worker: AndroidWorker,
    slots: Vec<AndroidWorkerSlot>,
    local_port: u16,
    locks: Vec<Child>,
    tunnel: Option<Child>,
    started_units: Vec<String>,
    wrapper_dir: Option<PathBuf>,
    dry_run: bool,
}

impl AndroidLease {
    pub fn acquire(config_path: Option<&Path>, names: &[String], dry_run: bool) -> Result<Self> {
        let config = load_worker_config(config_path)?;
        let (worker, slots) = select_android_slots(&config, names)?;
        let slots = slots.into_iter().cloned().collect::<Vec<_>>();
        let first_slot_index = worker
            .slots
            .iter()
            .position(|slot| slot.name == slots[0].name)
            .expect("selected slot exists on worker");
        let local_port = worker
            .local_port
            .checked_add(u16::try_from(first_slot_index)?)
            .context("Android worker local port range exceeds 65535")?;
        let mut lease = Self {
            worker: worker.clone(),
            slots,
            local_port,
            locks: Vec::new(),
            tunnel: None,
            started_units: Vec::new(),
            wrapper_dir: None,
            dry_run,
        };
        lease.lock_slots()?;
        lease.start_units()?;
        lease.start_tunnel()?;
        Ok(lease)
    }

    pub fn apply_to_android_config(&self, config: &mut AndroidConfig) -> Result<()> {
        if self.slots.len() != 1 {
            bail!("game Android commands require exactly one --slot");
        }
        config.target = crate::config::AndroidTarget::Device;
        config.adb_serial = Some(self.slots[0].adb_serial.clone());
        config.adb_host = Some("127.0.0.1".into());
        config.adb_port = Some(self.local_port);
        Ok(())
    }

    pub fn configure_command(&mut self, command: &mut Command) -> Result<()> {
        let adb = find_adb()?;
        self.configure_command_with_adb(command, &adb)
    }

    fn configure_command_with_adb(&mut self, command: &mut Command, adb: &Path) -> Result<()> {
        let wrapper = self.create_adb_wrapper_with(adb)?;
        let wrapper_dir = wrapper.parent().expect("adb wrapper has a parent");
        let path = env::join_paths(
            std::iter::once(wrapper_dir.to_path_buf())
                .chain(env::split_paths(&env::var_os("PATH").unwrap_or_default())),
        )?;
        command.env("PATH", path);
        command.env("ADB", &wrapper);
        command.env("ANDROID_SERIAL", &self.slots[0].adb_serial);
        for (index, slot) in self.slots.iter().enumerate() {
            command.env(format!("FRAGPIPE_ANDROID_SERIAL_{index}"), &slot.adb_serial);
        }
        Ok(())
    }

    fn lock_slots(&mut self) -> Result<()> {
        for slot_name in sorted_slot_names(&self.slots) {
            let lock_path = self.worker.lock_dir.join(format!("{slot_name}.lock"));
            // The holder exits on SSH stdin EOF, independently of local destructors.
            let inner = "printf 'FRAGPIPE_LOCKED\\n'; exec cat >/dev/null";
            let command = format!(
                "exec flock -n {} sh -c {}",
                shell_quote(lock_path.to_string_lossy()),
                shell_quote(inner)
            );
            println!(
                "[android-worker] lock {} on {}",
                slot_name, self.worker.host
            );
            if self.dry_run {
                continue;
            }
            let mut command = ssh_command(&self.worker.host, &command);
            command.stdout(Stdio::piped()).stdin(Stdio::piped());
            let mut child = command
                .spawn()
                .with_context(|| format!("failed to acquire Android slot `{slot_name}`"))?;
            let mut marker = String::new();
            if let Some(stdout) = child.stdout.take()
                && let Err(error) = BufReader::new(stdout).read_line(&mut marker)
            {
                kill_child(&mut child);
                return Err(error).context("failed to read Android slot lock response");
            }
            if marker.trim() != "FRAGPIPE_LOCKED" {
                let status = child.try_wait().ok().flatten();
                kill_child(&mut child);
                bail!(
                    "Android slot `{slot_name}` is already leased or lock failed{}",
                    status.map_or_else(String::new, |status| format!(" with {status}"))
                );
            }
            self.locks.push(child);
        }
        Ok(())
    }

    fn start_units(&mut self) -> Result<()> {
        for slot in &self.slots {
            let Some(unit) = slot.systemd_unit.as_deref() else {
                continue;
            };
            let command = format!("systemctl restart {}", shell_quote(unit));
            println!("[android-worker] restart {} on {}", unit, self.worker.host);
            if !self.dry_run {
                run_remote(&self.worker.host, &command)?;
            }
            self.started_units.push(unit.to_string());
        }
        Ok(())
    }

    fn start_tunnel(&mut self) -> Result<()> {
        println!(
            "[android-worker] tunnel 127.0.0.1:{} to {}:127.0.0.1:{}",
            self.local_port, self.worker.host, self.worker.adb_server_port
        );
        if self.dry_run {
            return Ok(());
        }
        let forward = format!(
            "127.0.0.1:{}:127.0.0.1:{}",
            self.local_port, self.worker.adb_server_port
        );
        let mut command = Command::new("ssh");
        command
            .args(["-N", "-o", "ExitOnForwardFailure=yes", "-L"])
            .arg(forward)
            .arg(&self.worker.host)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .process_group(0);
        terminate_with_parent(&mut command);
        let mut child = command
            .spawn()
            .with_context(|| format!("failed to start ADB tunnel to {}", self.worker.host))?;
        std::thread::sleep(std::time::Duration::from_millis(200));
        match child.try_wait() {
            Ok(Some(status)) => bail!("ADB tunnel to {} exited with {status}", self.worker.host),
            Ok(None) => {}
            Err(error) => {
                kill_child(&mut child);
                return Err(error).context("failed to poll ADB tunnel");
            }
        }
        self.tunnel = Some(child);
        Ok(())
    }

    fn create_adb_wrapper_with(&mut self, adb: &Path) -> Result<PathBuf> {
        if let Some(dir) = &self.wrapper_dir {
            return Ok(dir.join("adb"));
        }
        let dir = env::temp_dir().join(format!("fragpipe-android-{}", std::process::id()));
        if dir.exists() {
            fs::remove_dir_all(&dir)
                .with_context(|| format!("failed to reset {}", dir.display()))?;
        }
        fs::create_dir(&dir).with_context(|| format!("failed to create {}", dir.display()))?;
        let wrapper = dir.join("adb");
        fs::write(
            &wrapper,
            format!(
                "#!/bin/sh\nexec {} -H 127.0.0.1 -P {} \"$@\"\n",
                shell_quote(adb.to_string_lossy()),
                self.local_port
            ),
        )
        .with_context(|| format!("failed to write {}", wrapper.display()))?;
        fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("failed to make {} executable", wrapper.display()))?;
        self.wrapper_dir = Some(dir);
        Ok(wrapper)
    }
}

impl Drop for AndroidLease {
    fn drop(&mut self) {
        for unit in self.started_units.iter().rev() {
            let command = format!("systemctl stop {}", shell_quote(unit));
            println!("[android-worker] stop {} on {}", unit, self.worker.host);
            if !self.dry_run
                && let Err(error) = run_remote(&self.worker.host, &command)
            {
                eprintln!("[android-worker] failed to stop {unit}: {error:#}");
            }
        }
        if let Some(tunnel) = self.tunnel.as_mut() {
            kill_child(tunnel);
        }
        for lock in self.locks.iter_mut().rev() {
            kill_child(lock);
        }
        if let Some(dir) = &self.wrapper_dir {
            let _ = fs::remove_dir_all(dir);
        }
    }
}

pub fn run_android_with(
    config_path: Option<&Path>,
    slots: &[String],
    command: &[String],
    dry_run: bool,
) -> Result<()> {
    if command.is_empty() {
        bail!("android-with requires a command after --");
    }
    let mut lease = AndroidLease::acquire(config_path, slots, dry_run)?;
    println!("==> Android worker command: {}", shell_args(command));
    if dry_run {
        return Ok(());
    }
    let mut child = Command::new(&command[0]);
    child.args(&command[1..]);
    lease.configure_command(&mut child)?;
    let status = child
        .status()
        .context("failed to run Android worker command")?;
    if status.success() {
        Ok(())
    } else {
        Err(AndroidCommandError(status).into())
    }
}

fn ssh_command(host: &str, command: &str) -> Command {
    let mut ssh = Command::new("ssh");
    ssh.arg(host)
        .arg(format!("sh -lc {}", shell_quote(command)))
        .stdin(Stdio::null())
        .stderr(Stdio::inherit())
        .process_group(0);
    terminate_with_parent(&mut ssh);
    ssh
}

fn sorted_slot_names(slots: &[AndroidWorkerSlot]) -> Vec<String> {
    let mut names = slots
        .iter()
        .map(|slot| slot.name.clone())
        .collect::<Vec<_>>();
    names.sort();
    names
}

fn terminate_with_parent(command: &mut Command) {
    #[cfg(target_os = "linux")]
    {
        let parent = rustix::process::getpid();
        // SAFETY: only async-signal-safe rustix syscalls run between fork and exec.
        unsafe {
            command.pre_exec(move || {
                rustix::process::set_parent_process_death_signal(Some(
                    rustix::process::Signal::TERM,
                ))?;
                if rustix::process::getppid() != Some(parent) {
                    return Err(io::Error::other(
                        "fragpipe parent exited before child setup",
                    ));
                }
                Ok(())
            });
        }
    }
}

fn run_remote(host: &str, command: &str) -> Result<()> {
    let status = ssh_command(host, command)
        .status()
        .with_context(|| format!("failed to run command on {host}"))?;
    ensure_success(status, "remote Android worker command")
}

fn find_adb() -> Result<PathBuf> {
    if let Some(root) = env::var_os("ANDROID_SDK_ROOT").or_else(|| env::var_os("ANDROID_HOME")) {
        let candidate = PathBuf::from(root).join("platform-tools/adb");
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    for dir in env::split_paths(&env::var_os("PATH").unwrap_or_default()) {
        let candidate = dir.join("adb");
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    bail!("adb not found; run fragpipe inside an Android development shell")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn worker_config() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workers.toml");
        fs::write(
            &path,
            r#"
            [[android_worker]]
            name = "nomad"
            host = "dnomad"
            local_port = 15037
            lock_dir = "/run/lock/canix/android-worker"

            [[android_worker.slots]]
            name = "nomad-aosp35-0"
            kind = "emulator"
            adb_serial = "emulator-5554"
            systemd_unit = "canix-android-aosp35-0.service"
            "#,
        )
        .unwrap();
        (dir, path)
    }

    #[test]
    fn android_with_requires_a_command() {
        assert!(run_android_with(None, &[], &[], true).is_err());
    }

    #[test]
    fn dry_run_lease_applies_remote_adb_endpoint() {
        let (_dir, path) = worker_config();
        let slots = vec!["nomad-aosp35-0".into()];
        let lease = AndroidLease::acquire(Some(&path), &slots, true).unwrap();
        let mut config: AndroidConfig = toml::from_str(
            r#"
            apk_path = "app.apk"
            package_name = "com.example"
            "#,
        )
        .unwrap();
        lease.apply_to_android_config(&mut config).unwrap();
        assert_eq!(config.target, crate::config::AndroidTarget::Device);
        assert_eq!(config.adb_serial.as_deref(), Some("emulator-5554"));
        assert_eq!(config.adb_host.as_deref(), Some("127.0.0.1"));
        assert_eq!(config.adb_port, Some(15037));
    }

    #[test]
    fn separate_slots_use_separate_local_tunnel_ports() {
        let (_dir, path) = worker_config();
        let mut text = fs::read_to_string(&path).unwrap();
        text.push_str(
            r#"
            [[android_worker.slots]]
            name = "nomad-aosp35-1"
            kind = "emulator"
            adb_serial = "emulator-5558"
            systemd_unit = "canix-android-aosp35-1.service"
            "#,
        );
        fs::write(&path, text).unwrap();
        let slots = vec!["nomad-aosp35-1".into()];
        let lease = AndroidLease::acquire(Some(&path), &slots, true).unwrap();
        assert_eq!(lease.local_port, 15038);
    }

    #[test]
    fn requested_order_sets_environment_but_locks_sort() {
        let (_dir, path) = worker_config();
        let mut text = fs::read_to_string(&path).unwrap();
        text.push_str(
            r#"
            [[android_worker.slots]]
            name = "nomad-aosp35-1"
            kind = "emulator"
            adb_serial = "emulator-5558"
            systemd_unit = "canix-android-aosp35-1.service"
            "#,
        );
        fs::write(&path, text).unwrap();
        let slots = vec!["nomad-aosp35-1".into(), "nomad-aosp35-0".into()];
        let mut lease = AndroidLease::acquire(Some(&path), &slots, true).unwrap();
        let adb = path.parent().unwrap().join("adb-real");
        fs::write(&adb, "").unwrap();
        let mut command = Command::new("true");
        lease
            .configure_command_with_adb(&mut command, &adb)
            .unwrap();
        let env = command
            .get_envs()
            .map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    value.unwrap().to_string_lossy().into_owned(),
                )
            })
            .collect::<std::collections::HashMap<_, _>>();
        assert_eq!(env["ANDROID_SERIAL"], "emulator-5558");
        assert_eq!(env["FRAGPIPE_ANDROID_SERIAL_0"], "emulator-5558");
        assert_eq!(env["FRAGPIPE_ANDROID_SERIAL_1"], "emulator-5554");
        assert_eq!(
            sorted_slot_names(&lease.slots),
            ["nomad-aosp35-0", "nomad-aosp35-1"]
        );
    }

    #[test]
    fn multiple_slots_cannot_apply_to_game_config() {
        let (_dir, path) = worker_config();
        let mut text = fs::read_to_string(&path).unwrap();
        text.push_str(
            r#"
            [[android_worker.slots]]
            name = "nomad-aosp35-1"
            kind = "emulator"
            adb_serial = "emulator-5558"
            systemd_unit = "canix-android-aosp35-1.service"
            "#,
        );
        fs::write(&path, text).unwrap();
        let slots = vec!["nomad-aosp35-0".into(), "nomad-aosp35-1".into()];
        let lease = AndroidLease::acquire(Some(&path), &slots, true).unwrap();
        let mut config: AndroidConfig = toml::from_str(
            r#"
            apk_path = "app.apk"
            package_name = "com.example"
            "#,
        )
        .unwrap();
        assert!(lease.apply_to_android_config(&mut config).is_err());
    }

    #[test]
    fn android_with_dry_run_does_not_execute_command() {
        let (_dir, path) = worker_config();
        let slots = vec!["nomad-aosp35-0".into()];
        run_android_with(
            Some(&path),
            &slots,
            &["definitely-not-a-command".into()],
            true,
        )
        .unwrap();
    }
}
