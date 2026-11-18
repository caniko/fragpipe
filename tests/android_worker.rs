#![cfg(target_os = "linux")]

use rustix::process::Signal;
use std::fs;
use std::io::Write as _;
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
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
            .env("TMPDIR", self._dir.path())
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

fn until(what: &Path, ok: impl Fn() -> bool) {
    let started = Instant::now();
    while !ok() {
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "timed out: {what:?}"
        );
        thread::sleep(Duration::from_millis(25));
    }
}

fn released(path: &Path) {
    // Existing inode only, on an independent OFD; read-write so an exclusive
    // flock also works where it needs a writable descriptor (NFS). The
    // temporary Command and its stdin File both drop before any retry starts.
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    assert!(
        Command::new("flock")
            .args(["-w", "60", "0"])
            .stdin(file)
            .status()
            .unwrap()
            .success()
    );
}

fn hold_lock(path: &Path) -> fs::File {
    let f = fs::File::create_new(path).unwrap();
    f.try_lock().unwrap();
    f
}

fn echo(f: &Fixture, input: &mut ChildStdin, bytes: &[u8]) {
    let path = f._dir.path().join("echo");
    input.write_all(&bytes[bytes.len() - 1..]).unwrap();
    until(&path, || fs::read(&path).is_ok_and(|v| v == bytes));
}

/// Kills and reaps a controller the test did not reap itself, also on unwind,
/// so a failed readiness wait cannot leave it holding CI's stderr.
struct Lease(Child);

impl Drop for Lease {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn first(f: &Fixture) -> (Lease, ChildStdin) {
    let d = f._dir.path();
    let child = f
        .fragpipe()
        .args(["--slot", "slot-0", "--", "tee"])
        .arg(d.join("echo"))
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let mut lease = Lease(child);
    let mut input = lease.0.stdin.take().unwrap();
    echo(f, &mut input, b"A");
    (lease, input)
}

fn kill_and_release(f: &Fixture, lease: &mut Lease) {
    lease.0.kill().unwrap();
    let status = lease.0.wait().unwrap();
    assert_eq!(status.signal(), Some(Signal::KILL.as_raw()));
    released(&f.lock("slot-0"));
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
    let _holder = hold_lock(&second);
    let out = fixture
        .fragpipe()
        .args(["--slot", "slot-0", "--slot", "slot-1", "--", "true"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("slot `slot-1` is already leased"),
        "{stderr}"
    );
    released(&fixture.lock("slot-0"));
}

#[test]
fn competing_lease_is_rejected() {
    let fixture = Fixture::new(1);
    let (mut first, mut input) = first(&fixture);
    let output = fixture
        .fragpipe()
        .args(["--slot", "slot-0", "--", "true"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("already leased"));
    kill_and_release(&fixture, &mut first);
    echo(&fixture, &mut input, b"AB");
}

#[test]
fn command_exit_code_is_propagated() {
    let fixture = Fixture::new(1);
    let status = run_one_slot(&fixture, &["sh", "-c", "exit 42"]);
    assert_eq!(status.code(), Some(42));
    released(&fixture.lock("slot-0"));
}

#[test]
fn killing_controller_releases_remote_lock() {
    let fixture = Fixture::new(1);
    let (mut controller, mut input) = first(&fixture);
    kill_and_release(&fixture, &mut controller);
    assert!(run_one_slot(&fixture, &["true"]).success());
    echo(&fixture, &mut input, b"AB");
}
