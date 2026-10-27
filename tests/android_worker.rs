#![cfg(target_os = "linux")]

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

struct Fixture {
    _dir: tempfile::TempDir,
    config: PathBuf,
    path: std::ffi::OsString,
    lock_dir: PathBuf,
}

impl Fixture {
    fn new(slot_count: usize) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let lock_dir = dir.path().join("locks");
        fs::create_dir(&lock_dir).unwrap();
        let bin = dir.path().join("bin");
        fs::create_dir(&bin).unwrap();
        write_executable(
            &bin.join("ssh"),
            r#"#!/bin/sh
if [ "$1" = "-N" ]; then
    trap 'exit 0' TERM INT
    while sleep 1; do :; done
fi
shift
exec sh -c "$1"
"#,
        );
        write_executable(&bin.join("adb"), "#!/bin/sh\nexit 0\n");

        let mut config = format!(
            r#"
[[android_worker]]
name = "local"
host = "fake-host"
local_port = 15037
lock_dir = {:?}
"#,
            lock_dir
        );
        for index in 0..slot_count {
            config.push_str(&format!(
                r#"
[[android_worker.slots]]
name = "slot-{index}"
kind = "device"
adb_serial = "serial-{index}"
"#
            ));
        }
        let config_path = dir.path().join("workers.toml");
        fs::write(&config_path, config).unwrap();
        let path = std::env::join_paths(std::iter::once(bin).chain(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        )))
        .unwrap();
        Self {
            _dir: dir,
            config: config_path,
            path,
            lock_dir,
        }
    }

    fn fragpipe(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_fragpipe"));
        command
            .env("PATH", &self.path)
            .args(["android-with", "--workers-config"])
            .arg(&self.config);
        command
    }

    fn lock(&self, slot: &str) -> PathBuf {
        self.lock_dir.join(format!("{slot}.lock"))
    }
}

fn write_executable(path: &Path, text: &str) {
    fs::write(path, text).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

fn lock_is_free(path: &Path) -> bool {
    Command::new("flock")
        .arg("-n")
        .arg(path)
        .arg("true")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap()
        .success()
}

fn wait_for(path: &Path, expected_free: bool) {
    let started = Instant::now();
    while lock_is_free(path) != expected_free {
        assert!(started.elapsed() < Duration::from_secs(5));
        thread::sleep(Duration::from_millis(25));
    }
}

fn hold_lock(path: &Path) -> Child {
    let child = Command::new("flock")
        .arg(path)
        .args(["sleep", "30"])
        .spawn()
        .unwrap();
    wait_for(path, false);
    child
}

fn run_one_slot(fixture: &Fixture, command: &[&str]) -> ExitStatus {
    fixture
        .fragpipe()
        .args(["--slot", "slot-0", "--"])
        .args(command)
        .status()
        .unwrap()
}

#[test]
fn partial_acquisition_releases_earlier_locks() {
    let fixture = Fixture::new(2);
    let second = fixture.lock("slot-1");
    let mut holder = hold_lock(&second);
    let status = fixture
        .fragpipe()
        .args(["--slot", "slot-0", "--slot", "slot-1", "--", "true"])
        .status()
        .unwrap();
    assert!(!status.success());
    assert!(lock_is_free(&fixture.lock("slot-0")));
    holder.kill().unwrap();
    holder.wait().unwrap();
}

#[test]
fn competing_lease_is_rejected() {
    let fixture = Fixture::new(1);
    let mut first = fixture
        .fragpipe()
        .args(["--slot", "slot-0", "--", "sleep", "30"])
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    wait_for(&fixture.lock("slot-0"), false);
    let output = fixture
        .fragpipe()
        .args(["--slot", "slot-0", "--", "true"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("already leased"));
    first.kill().unwrap();
    first.wait().unwrap();
    wait_for(&fixture.lock("slot-0"), true);
}

#[test]
fn command_exit_code_is_propagated() {
    let fixture = Fixture::new(1);
    let status = run_one_slot(&fixture, &["sh", "-c", "exit 42"]);
    assert_eq!(status.code(), Some(42));
    assert!(lock_is_free(&fixture.lock("slot-0")));
}

#[test]
fn killing_controller_releases_remote_lock() {
    let fixture = Fixture::new(1);
    let mut controller = fixture
        .fragpipe()
        .args(["--slot", "slot-0", "--", "sleep", "30"])
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    wait_for(&fixture.lock("slot-0"), false);
    controller.kill().unwrap();
    controller.wait().unwrap();
    wait_for(&fixture.lock("slot-0"), true);
    assert!(run_one_slot(&fixture, &["true"]).success());
}
