use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use rustix::fd::OwnedFd;
use rustix::io::Errno;
use rustix::process::{Pid, PidfdFlags, Signal, pidfd_open, pidfd_send_signal};
use serde::{Deserialize, Serialize};

use crate::config::{Config, RemotePeer, binary_file_name, project_root, resolve_path};
use crate::process::ensure_success;
use crate::util::{shell_args, shell_quote};

const GATE_SENTINEL: &str = "FRAGPIPE_SSH_GATE_V1";
const MAX_REQUEST_BYTES: u64 = 1024 * 1024;
const MAX_LOG_BYTES: u64 = 1024 * 1024;

pub fn deploy(config: &Config, remote: &RemotePeer, dry_run: bool) -> Result<()> {
    let project_root = project_root(config);
    if remote.restricted {
        run_gate_request(remote, &GateRequest::Prepare, dry_run, true)?;
    } else {
        run_ssh(
            &remote.host,
            &format!("mkdir -p {}", shell_quote(&remote.remote_dir)),
            dry_run,
        )?;
    }

    let binary_name = remote_binary_name(config, remote)?;
    let remote_binary = format!("{}/{}", remote.remote_dir, binary_name);
    run_rsync(
        &config.game.binary,
        remote,
        &remote_binary,
        project_root,
        dry_run,
    )?;

    if let Some(assets_dir) = config.game.assets_dir.as_ref() {
        let target = remote
            .assets_dir
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_else(|| format!("{}/assets", remote.remote_dir));
        run_rsync(assets_dir, remote, &target, project_root, dry_run)?;
    }

    for item in &remote.deploy {
        run_rsync(&item.source, remote, &item.target, project_root, dry_run)?;
    }

    if remote.restricted {
        run_gate_request(remote, &GateRequest::Prepare, dry_run, true).map(|_| ())
    } else {
        run_ssh(
            &remote.host,
            &format!("chmod +x {}", shell_quote(&remote_binary)),
            dry_run,
        )
    }
}

pub fn launch_remote(
    config: &Config,
    remote: &RemotePeer,
    args: &[String],
    dry_run: bool,
) -> Result<()> {
    if remote.restricted {
        let request = GateRequest::Launch {
            args: args.to_vec(),
            env: config
                .game
                .env
                .iter()
                .chain(&remote.env)
                .map(|pair| GateEnv {
                    name: pair.name.clone(),
                    value: pair.value.clone(),
                })
                .collect(),
        };
        println!(
            "==> Remote joining peer on {} via restricted SSH gate",
            remote.name
        );
        run_gate_request(remote, &request, dry_run, true)?;
        return Ok(());
    }
    let command = launch_command(config, remote, args)?;
    println!("==> Remote joining peer on {}: {}", remote.name, command);
    run_ssh(&remote.host, &command, dry_run)
}

fn launch_command(config: &Config, remote: &RemotePeer, args: &[String]) -> Result<String> {
    launch_command_with_files(config, remote, args, &remote.log_file, &remote.pid_file)
}

fn launch_command_with_files(
    config: &Config,
    remote: &RemotePeer,
    args: &[String],
    log_file: &str,
    pid_file: &str,
) -> Result<String> {
    let binary_name = remote_binary_name(config, remote)?;
    let mut env = String::new();
    for pair in &config.game.env {
        env.push_str(&format!("{}={} ", pair.name, shell_quote(&pair.value)));
    }
    for pair in &remote.env {
        env.push_str(&format!("{}={} ", pair.name, shell_quote(&pair.value)));
    }

    let remote_binary = format!("{}/{}", remote.remote_dir, binary_name);
    let remote_pid = format!("{}/{}", remote.remote_dir, pid_file);
    let run = format!(
        "{} {}; _fragpipe_status=$?; printf '\\nFRAGPIPE_REMOTE_EXIT=%s\\n' \"$_fragpipe_status\"",
        shell_quote(&remote_binary),
        shell_args(args),
    );
    let command = format!(
        "cd {} || exit 1; {}nohup sh -lc {} > {} 2>&1 < /dev/null & printf '%s\\n' $! > {}",
        shell_quote(&remote.remote_dir),
        env,
        shell_quote(run),
        shell_quote(log_file),
        shell_quote(&remote_pid),
    );
    Ok(command)
}

pub fn stop_remote(config: &Config, remote: &RemotePeer, dry_run: bool) -> Result<()> {
    if remote.restricted {
        run_gate_request(remote, &GateRequest::Stop, dry_run, true)?;
        return Ok(());
    }
    let kill_name = crate::config::kill_name(config)?;
    let remote_pid = format!("{}/{}", remote.remote_dir, remote.pid_file);
    let binary_name = remote_binary_name(config, remote)?;
    let remote_binary = format!("{}/{}", remote.remote_dir, binary_name);
    run_ssh(
        &remote.host,
        &format!(
            "if test -s {pid}; then _fragpipe_pid=$(cat {pid}); pkill -P \"$_fragpipe_pid\" 2>/dev/null || true; kill \"$_fragpipe_pid\" 2>/dev/null || true; fi; rm -f {pid}; pkill -f {binary_pattern} 2>/dev/null || true; pkill -x {name} 2>/dev/null || true",
            pid = shell_quote(&remote_pid),
            binary_pattern = shell_quote(format!("^{remote_binary} ")),
            name = shell_quote(kill_name),
        ),
        dry_run,
    )
}

pub fn clear_remote_log(remote: &RemotePeer, dry_run: bool) -> Result<()> {
    if remote.restricted {
        run_gate_request(remote, &GateRequest::ClearLog, dry_run, true)?;
        return Ok(());
    }
    run_ssh(
        &remote.host,
        &format!(
            "rm -f {}/{}",
            shell_quote(&remote.remote_dir),
            shell_quote(&remote.log_file)
        ),
        dry_run,
    )
}

pub fn remote_log(remote: &RemotePeer) -> Result<String> {
    if remote.restricted {
        let output = run_gate_request(remote, &GateRequest::ReadLog, false, false)?;
        return Ok(String::from_utf8_lossy(&output).into_owned());
    }
    let command = format!(
        "tail -n 200 {}/{} 2>/dev/null || true",
        shell_quote(&remote.remote_dir),
        shell_quote(&remote.log_file),
    );
    let command = remote_shell(&command);
    let output = Command::new("ssh")
        .arg(&remote.host)
        .arg(command)
        .output()
        .with_context(|| format!("failed to read remote log from {}", remote.host))?;
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[derive(Debug)]
pub struct RemoteInstanceSnapshot {
    pub log: String,
    pub running: bool,
    pub exit_code: Option<i32>,
}

pub fn launch_remote_instance(
    config: &Config,
    remote: &RemotePeer,
    index: u8,
    args: &[String],
    dry_run: bool,
) -> Result<()> {
    if remote.restricted {
        bail!("multiple remote processes are not supported by restricted SSH peers");
    }
    let (log_file, pid_file) = instance_files(index);
    let command = launch_command_with_files(config, remote, args, &log_file, &pid_file)?;
    println!(
        "==> Remote tournament peer {index} on {}: {command}",
        remote.name
    );
    run_ssh(&remote.host, &command, dry_run)
}

pub fn stop_remote_instance(remote: &RemotePeer, index: u8, dry_run: bool) -> Result<()> {
    if remote.restricted {
        bail!("multiple remote processes are not supported by restricted SSH peers");
    }
    run_ssh(&remote.host, &stop_instance_command(remote, index), dry_run)
}

pub fn clear_remote_instance_log(remote: &RemotePeer, index: u8, dry_run: bool) -> Result<()> {
    let (log_file, _) = instance_files(index);
    run_ssh(
        &remote.host,
        &format!(
            "rm -f {}",
            shell_quote(format!("{}/{}", remote.remote_dir, log_file))
        ),
        dry_run,
    )
}

pub fn remote_instance_snapshot(remote: &RemotePeer, index: u8) -> Result<RemoteInstanceSnapshot> {
    let (log_file, pid_file) = instance_files(index);
    let log_path = shell_quote(format!("{}/{}", remote.remote_dir, log_file));
    let pid_path = shell_quote(format!("{}/{}", remote.remote_dir, pid_file));
    let command = format!(
        "tail -n 200 {log_path} 2>/dev/null || true; printf '\\nFRAGPIPE_REMOTE_RUNNING='; if test -s {pid_path}; then _fragpipe_pid=$(cat {pid_path}); kill -0 \"$_fragpipe_pid\" 2>/dev/null && printf 1 || printf 0; else printf 0; fi"
    );
    let output = run_ssh_output(&remote.host, &command)?;
    let text = String::from_utf8_lossy(&output.stdout);
    let Some((log, running)) = text.rsplit_once("\nFRAGPIPE_REMOTE_RUNNING=") else {
        bail!("remote peer {index} status response was malformed");
    };
    Ok(RemoteInstanceSnapshot {
        log: log.to_string(),
        running: running.trim() == "1",
        exit_code: remote_exit_code(log),
    })
}

pub fn remote_instance_log(remote: &RemotePeer, index: u8) -> Result<String> {
    let (log_file, _) = instance_files(index);
    let command = format!(
        "cat {} 2>/dev/null || true",
        shell_quote(format!("{}/{}", remote.remote_dir, log_file))
    );
    let output = run_ssh_output(&remote.host, &command)?;
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn instance_files(index: u8) -> (String, String) {
    (
        format!("fragpipe-peer-{index}.log"),
        format!(".fragpipe-peer-{index}.pid"),
    )
}

fn remote_exit_code(log: &str) -> Option<i32> {
    log.lines()
        .rev()
        .find_map(|line| line.strip_prefix("FRAGPIPE_REMOTE_EXIT="))?
        .parse()
        .ok()
}

fn stop_instance_command(remote: &RemotePeer, index: u8) -> String {
    let (_, pid_file) = instance_files(index);
    let pid = shell_quote(format!("{}/{}", remote.remote_dir, pid_file));
    format!(
        "if test -s {pid}; then _fragpipe_pid=$(cat {pid}); pkill -P \"$_fragpipe_pid\" 2>/dev/null || true; kill \"$_fragpipe_pid\" 2>/dev/null || true; fi; rm -f {pid}"
    )
}

fn run_ssh(host: &str, command: &str, dry_run: bool) -> Result<()> {
    let command = remote_shell(command);
    println!("ssh {host} {command}");
    if dry_run {
        return Ok(());
    }
    let status = Command::new("ssh")
        .arg(host)
        .arg(&command)
        .status()
        .with_context(|| format!("failed to run ssh command on {host}"))?;
    ensure_success(status, "ssh command")
}

fn run_ssh_output(host: &str, command: &str) -> Result<std::process::Output> {
    let output = Command::new("ssh")
        .arg(host)
        .arg(remote_shell(command))
        .output()
        .with_context(|| format!("failed to run SSH command on {host}"))?;
    ensure_success(output.status, "ssh command")?;
    Ok(output)
}

/// Fleetix user shells are Nushell on the game hosts, while Fragpipe's
/// lifecycle snippets intentionally use portable POSIX utilities and syntax.
/// Force a POSIX shell so options such as `mkdir -p` and redirections retain
/// their meaning over SSH.
fn remote_shell(command: &str) -> String {
    format!("sh -lc {}", shell_quote(command))
}

fn run_rsync(
    source: &Path,
    remote: &RemotePeer,
    target: &str,
    project_root: &Path,
    dry_run: bool,
) -> Result<()> {
    let resolved = resolve_path(project_root, source);
    let target = if remote.restricted {
        restricted_target(remote, target)?
    } else {
        target.to_owned()
    };
    println!("rsync -az {} {}:{target}", resolved.display(), remote.host);
    if dry_run {
        return Ok(());
    }
    let status = Command::new("rsync")
        .arg("-az")
        .arg(&resolved)
        .arg(format!("{}:{target}", remote.host))
        .status()
        .context("failed to run rsync")?;
    ensure_success(status, "rsync")
}

fn remote_binary_name(config: &Config, remote: &RemotePeer) -> Result<String> {
    match remote.binary_name.as_ref() {
        Some(name) => Ok(name.clone()),
        None => binary_file_name(&config.game.binary),
    }
}

fn restricted_target(remote: &RemotePeer, target: &str) -> Result<String> {
    let root = Path::new(&remote.remote_dir);
    if !root.is_absolute() {
        bail!("restricted remote_dir must be absolute");
    }
    let target = Path::new(target);
    let relative = if target.is_absolute() {
        target.strip_prefix(root).with_context(|| {
            format!(
                "restricted deploy target {} is outside {}",
                target.display(),
                root.display()
            )
        })?
    } else {
        target
    };
    validate_relative_path(relative)?;
    Ok(if relative.as_os_str().is_empty() {
        ".".into()
    } else {
        relative.to_string_lossy().into_owned()
    })
}

fn run_gate_request(
    remote: &RemotePeer,
    request: &GateRequest,
    dry_run: bool,
    print_command: bool,
) -> Result<Vec<u8>> {
    if print_command {
        println!("ssh {} {GATE_SENTINEL}", remote.host);
    }
    if dry_run {
        return Ok(Vec::new());
    }

    let input = serde_json::to_vec(request)?;
    let mut child = Command::new("ssh")
        .arg(&remote.host)
        .arg(GATE_SENTINEL)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to run restricted SSH request on {}", remote.host))?;
    let write_result = child
        .stdin
        .take()
        .context("restricted SSH stdin missing")?
        .write_all(&input);
    let output = child
        .wait_with_output()
        .context("failed to wait for restricted SSH request")?;
    write_result.context("failed to send restricted SSH request")?;
    ensure_success(output.status, "restricted SSH request")?;
    Ok(output.stdout)
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "kebab-case", deny_unknown_fields)]
enum GateRequest {
    Prepare,
    Launch {
        args: Vec<String>,
        env: Vec<GateEnv>,
    },
    Stop,
    ClearLog,
    ReadLog,
    Status,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GateEnv {
    name: String,
    value: String,
}

#[derive(Debug, PartialEq, Eq)]
enum GateCommand {
    Protocol,
    Rsync,
    Reject,
}

fn classify_gate_command(command: &str) -> GateCommand {
    if command == GATE_SENTINEL {
        GateCommand::Protocol
    } else if command == "rsync --server" || command.starts_with("rsync --server ") {
        GateCommand::Rsync
    } else {
        GateCommand::Reject
    }
}

pub fn run_gate(
    root: &Path,
    binary: &Path,
    log_file: &Path,
    pid_file: &Path,
    rrsync: &Path,
) -> Result<()> {
    let original = env::var("SSH_ORIGINAL_COMMAND").context("SSH_ORIGINAL_COMMAND is required")?;
    match classify_gate_command(&original) {
        GateCommand::Protocol => Gate::new(root, binary, log_file, pid_file)?
            .handle_protocol(std::io::stdin().lock(), std::io::stdout().lock()),
        GateCommand::Rsync => run_rrsync(root, rrsync),
        GateCommand::Reject => bail!("SSH command is not permitted"),
    }
}

fn run_rrsync(root: &Path, rrsync: &Path) -> Result<()> {
    if !root.is_absolute() {
        bail!("ssh-gate --root must be absolute");
    }
    fs::create_dir_all(root)
        .with_context(|| format!("failed to create restricted root {}", root.display()))?;
    let root = fs::canonicalize(root)
        .with_context(|| format!("failed to resolve restricted root {}", root.display()))?;
    if !rrsync.is_absolute() {
        bail!("ssh-gate --rrsync must be absolute");
    }
    let status = Command::new(rrsync)
        .arg("-wo")
        .arg("-munge")
        .arg("-no-del")
        .arg(&root)
        .status()
        .context("failed to run rrsync")?;
    ensure_success(status, "rrsync")
}

struct Gate {
    root: PathBuf,
    binary: PathBuf,
    log_file: PathBuf,
    pid_file: PathBuf,
}

impl Gate {
    fn new(root: &Path, binary: &Path, log_file: &Path, pid_file: &Path) -> Result<Self> {
        if !root.is_absolute() {
            bail!("ssh-gate --root must be absolute");
        }
        fs::create_dir_all(root)
            .with_context(|| format!("failed to create restricted root {}", root.display()))?;
        let root = fs::canonicalize(root)
            .with_context(|| format!("failed to resolve restricted root {}", root.display()))?;
        Ok(Self {
            binary: fixed_file(&root, binary)?,
            log_file: fixed_file(&root, log_file)?,
            pid_file: fixed_file(&root, pid_file)?,
            root,
        })
    }

    fn handle_protocol(&self, reader: impl Read, mut writer: impl Write) -> Result<()> {
        let mut input = Vec::new();
        reader.take(MAX_REQUEST_BYTES + 1).read_to_end(&mut input)?;
        if input.len() as u64 > MAX_REQUEST_BYTES {
            bail!("restricted SSH request is too large");
        }
        let value: serde_json::Value =
            serde_json::from_slice(&input).context("invalid restricted SSH request")?;
        let request: GateRequest =
            serde_json::from_value(value.clone()).context("invalid restricted SSH request")?;
        if serde_json::to_value(&request)? != value {
            bail!("restricted SSH request contains unknown fields");
        }
        match request {
            GateRequest::Prepare => self.prepare(),
            GateRequest::Launch { args, env } => self.launch(&args, &env),
            GateRequest::Stop => self.stop(),
            GateRequest::ClearLog => self.clear_log(),
            GateRequest::ReadLog => {
                writer.write_all(&self.read_log()?)?;
                Ok(())
            }
            GateRequest::Status => {
                serde_json::to_writer(&mut writer, &self.status()?)?;
                writer.write_all(b"\n")?;
                Ok(())
            }
        }
    }

    fn prepare(&self) -> Result<()> {
        if self.binary.exists() {
            reject_symlink(&self.binary)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                let mut permissions = fs::metadata(&self.binary)?.permissions();
                permissions.set_mode(permissions.mode() | 0o111);
                fs::set_permissions(&self.binary, permissions)?;
            }
        }
        Ok(())
    }

    fn launch(&self, args: &[String], env: &[GateEnv]) -> Result<()> {
        self.prepare()?;
        if !self.binary.is_file() {
            bail!("restricted binary is missing: {}", self.binary.display());
        }
        if let Some(process) = self.read_pid()? {
            if process.is_running()? {
                bail!("restricted process {} is already running", process.pid);
            }
            fs::remove_file(&self.pid_file)?;
        }
        reject_symlink(&self.log_file)?;
        reject_symlink(&self.pid_file)?;
        let log = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&self.log_file)
            .with_context(|| format!("failed to open {}", self.log_file.display()))?;
        let log_err = log.try_clone()?;
        let mut command = Command::new(&self.binary);
        command
            .args(args)
            .current_dir(&self.root)
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(log_err));
        for pair in env {
            command.env(&pair.name, &pair.value);
        }
        let mut child = command
            .spawn()
            .with_context(|| format!("failed to launch {}", self.binary.display()))?;
        let process = ProcessIdentity::from_pid(child.id())?;
        if let Err(error) = fs::write(
            &self.pid_file,
            format!("{} {}\n", process.pid, process.start_time),
        ) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error).context("failed to write restricted PID file");
        }
        Ok(())
    }

    fn stop(&self) -> Result<()> {
        let Some(process) = self.read_pid()? else {
            return Ok(());
        };
        if let Some(pidfd) = process.open_running()? {
            signal_pidfd(&pidfd, Signal::TERM)?;
            for _ in 0..50 {
                if !process.is_running()? {
                    break;
                }
                thread::sleep(Duration::from_millis(100));
            }
            if process.is_running()? {
                signal_pidfd(&pidfd, Signal::KILL)?;
                for _ in 0..10 {
                    if !process.is_running()? {
                        break;
                    }
                    thread::sleep(Duration::from_millis(100));
                }
            }
            if process.is_running()? {
                bail!("restricted process group {} did not stop", process.pid);
            }
        }
        fs::remove_file(&self.pid_file)
            .with_context(|| format!("failed to remove {}", self.pid_file.display()))
    }

    fn clear_log(&self) -> Result<()> {
        reject_symlink(&self.log_file)?;
        match fs::remove_file(&self.log_file) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error).with_context(|| {
                format!(
                    "failed to remove restricted log {}",
                    self.log_file.display()
                )
            }),
        }
    }

    fn read_log(&self) -> Result<Vec<u8>> {
        reject_symlink(&self.log_file)?;
        let mut file = match File::open(&self.log_file) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        let start = file.metadata()?.len().saturating_sub(MAX_LOG_BYTES);
        file.seek(SeekFrom::Start(start))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        if start > 0
            && let Some(newline) = bytes.iter().position(|byte| *byte == b'\n')
        {
            bytes.drain(..=newline);
        }
        let text = String::from_utf8_lossy(&bytes);
        let lines = text.lines().rev().take(200).collect::<Vec<_>>();
        let mut output = lines.into_iter().rev().collect::<Vec<_>>().join("\n");
        if !output.is_empty() {
            output.push('\n');
        }
        Ok(output.into_bytes())
    }

    fn status(&self) -> Result<GateStatus> {
        let process = self.read_pid()?;
        Ok(GateStatus {
            running: process
                .as_ref()
                .is_some_and(|process| process.is_running().unwrap_or(false)),
            pid: process.map(|process| process.pid),
        })
    }

    fn read_pid(&self) -> Result<Option<ProcessIdentity>> {
        reject_symlink(&self.pid_file)?;
        let text = match fs::read_to_string(&self.pid_file) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let mut fields = text.split_whitespace();
        let pid = fields
            .next()
            .context("restricted PID file is malformed")?
            .parse::<u32>()
            .context("restricted PID file is malformed")?;
        if pid <= 1 {
            bail!("restricted PID file contains an unsafe PID");
        }
        let start_time = fields
            .next()
            .context("restricted PID file is missing the process start time")?
            .parse::<u64>()
            .context("restricted PID file has an invalid process start time")?;
        if fields.next().is_some() {
            bail!("restricted PID file has unexpected fields");
        }
        Ok(Some(ProcessIdentity { pid, start_time }))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ProcessIdentity {
    pid: u32,
    start_time: u64,
}

impl ProcessIdentity {
    fn from_pid(pid: u32) -> Result<Self> {
        let (_, start_time) = process_state_and_start_time(pid)?;
        Ok(Self { pid, start_time })
    }

    fn is_running(self) -> Result<bool> {
        match process_state_and_start_time(self.pid) {
            Ok((state, start_time)) => Ok(state != 'Z' && start_time == self.start_time),
            Err(_error) if !process_exists(self.pid) => Ok(false),
            Err(error) => Err(error),
        }
    }

    fn open_running(self) -> Result<Option<OwnedFd>> {
        let pid = Pid::from_raw(self.pid as i32).context("restricted PID is invalid")?;
        let pidfd = match pidfd_open(pid, PidfdFlags::empty()) {
            Ok(pidfd) => pidfd,
            Err(Errno::SRCH) => return Ok(None),
            Err(error) => return Err(error).context("failed to open restricted process handle"),
        };
        if self.is_running()? {
            Ok(Some(pidfd))
        } else {
            Ok(None)
        }
    }
}

#[derive(Debug, PartialEq, Eq, Serialize)]
struct GateStatus {
    running: bool,
    pid: Option<u32>,
}

fn fixed_file(root: &Path, relative: &Path) -> Result<PathBuf> {
    validate_relative_path(relative)?;
    if relative.components().count() != 1 {
        bail!("restricted binary, log, and PID paths must be file names");
    }
    Ok(root.join(relative))
}

fn validate_relative_path(path: &Path) -> Result<()> {
    if path.is_absolute() || path.as_os_str().is_empty() {
        bail!("restricted path must be non-empty and relative");
    }
    if path
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        bail!("restricted path contains a forbidden component");
    }
    Ok(())
}

fn reject_symlink(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            bail!("restricted path may not be a symlink: {}", path.display())
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn process_exists(pid: u32) -> bool {
    Path::new("/proc").join(pid.to_string()).exists()
}

fn process_state_and_start_time(pid: u32) -> Result<(char, u64)> {
    let stat = fs::read_to_string(Path::new("/proc").join(pid.to_string()).join("stat"))
        .with_context(|| format!("failed to read process {pid} start time"))?;
    let end = stat
        .rfind(')')
        .context("process stat is missing the command terminator")?;
    let fields = stat[end + 1..].split_whitespace().collect::<Vec<_>>();
    let state = fields
        .first()
        .and_then(|field| field.chars().next())
        .context("process stat is missing the state")?;
    let start_time = fields
        .get(19)
        .context("process stat is missing the start time")?
        .parse::<u64>()
        .context("process stat has an invalid start time")?;
    Ok((state, start_time))
}

fn signal_pidfd(pidfd: &OwnedFd, signal: Signal) -> Result<()> {
    match pidfd_send_signal(pidfd, signal) {
        Ok(()) | Err(Errno::SRCH) => Ok(()),
        Err(error) => Err(error).context("failed to signal restricted process"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DeployPath;
    use std::io::Cursor;
    use std::path::PathBuf;

    fn test_config() -> Config {
        toml::from_str(
            r#"
            [game]
            binary = "game"
            "#,
        )
        .unwrap()
    }

    fn test_remote() -> RemotePeer {
        RemotePeer {
            name: "test-peer".into(),
            host: "test-host".into(),
            remote_dir: "/remote".into(),
            restricted: false,
            log_file: "game.log".into(),
            pid_file: ".fragpipe.pid".into(),
            binary_name: None,
            assets_dir: None,
            deploy: vec![],
            env: vec![],
            join_args: vec![],
        }
    }

    #[test]
    fn deploy_dry_run_succeeds() {
        let config = test_config();
        let remote = test_remote();
        deploy(&config, &remote, true).unwrap();
    }

    #[test]
    fn remote_commands_are_explicitly_run_by_posix_shell() {
        let command = remote_shell("mkdir -p '/tmp/game dir'");
        assert!(command.starts_with("sh -lc "));
        assert!(command.contains("mkdir -p"));
    }

    #[test]
    fn deploy_dry_run_with_assets_dir() {
        let config: Config = toml::from_str(
            r#"
            [game]
            binary = "game"
            assets_dir = "assets"
            "#,
        )
        .unwrap();
        let remote = test_remote();
        deploy(&config, &remote, true).unwrap();
    }

    #[test]
    fn deploy_dry_run_with_custom_remote_assets() {
        let config: Config = toml::from_str(
            r#"
            [game]
            binary = "game"
            assets_dir = "assets"
            "#,
        )
        .unwrap();
        let mut remote = test_remote();
        remote.assets_dir = Some(PathBuf::from("/remote/custom-assets"));
        deploy(&config, &remote, true).unwrap();
    }

    #[test]
    fn deploy_dry_run_with_deploy_paths() {
        let config = test_config();
        let mut remote = test_remote();
        remote.deploy = vec![DeployPath {
            source: PathBuf::from("extra-file"),
            target: "/remote/extra-file".into(),
        }];
        deploy(&config, &remote, true).unwrap();
    }

    #[test]
    fn launch_remote_dry_run_succeeds() {
        let config = test_config();
        let remote = test_remote();
        launch_remote(&config, &remote, &[], true).unwrap();
    }

    #[test]
    fn launch_remote_dry_run_with_args() {
        let config = test_config();
        let remote = test_remote();
        launch_remote(&config, &remote, &["--join".into(), "addr".into()], true).unwrap();
    }

    #[test]
    fn launch_command_does_not_background_directory_change() {
        let config = test_config();
        let remote = test_remote();
        let command = launch_command(&config, &remote, &[]).unwrap();
        assert!(command.contains("cd /remote || exit 1;"));
        assert!(command.contains("FRAGPIPE_REMOTE_EXIT=%s"));
        assert!(!command.contains("&&"));
    }

    #[test]
    fn tournament_instances_use_distinct_pid_and_log_files() {
        assert_eq!(
            instance_files(4),
            ("fragpipe-peer-4.log".into(), ".fragpipe-peer-4.pid".into())
        );
        assert_ne!(instance_files(4), instance_files(5));
    }

    #[test]
    fn tournament_instance_stop_only_targets_its_pid() {
        let remote = test_remote();
        let command = stop_instance_command(&remote, 4);
        assert!(command.contains(".fragpipe-peer-4.pid"));
        assert!(!command.contains("pkill -x"));
        assert!(!command.contains("game"));
    }

    #[test]
    fn remote_exit_code_uses_last_status_marker() {
        assert_eq!(remote_exit_code("log\nFRAGPIPE_REMOTE_EXIT=1\n"), Some(1));
        assert_eq!(
            remote_exit_code("FRAGPIPE_REMOTE_EXIT=1\nFRAGPIPE_REMOTE_EXIT=0\n"),
            Some(0)
        );
        assert_eq!(remote_exit_code("log only"), None);
    }

    #[test]
    fn stop_remote_dry_run_succeeds() {
        let config = test_config();
        let remote = test_remote();
        stop_remote(&config, &remote, true).unwrap();
    }

    #[test]
    fn restricted_lifecycle_dry_run_succeeds() {
        let config = test_config();
        let mut remote = test_remote();
        remote.restricted = true;
        deploy(&config, &remote, true).unwrap();
        launch_remote(&config, &remote, &["argument with spaces".into()], true).unwrap();
        clear_remote_log(&remote, true).unwrap();
        stop_remote(&config, &remote, true).unwrap();
    }

    #[test]
    fn remote_binary_name_uses_explicit_name() {
        let config = test_config();
        let mut remote = test_remote();
        remote.binary_name = Some("custom-bin".into());
        let name = remote_binary_name(&config, &remote).unwrap();
        assert_eq!(name, "custom-bin");
    }

    #[test]
    fn remote_binary_name_falls_back_to_game_binary() {
        let config = test_config();
        let remote = test_remote();
        let name = remote_binary_name(&config, &remote).unwrap();
        assert_eq!(name, "game");
    }

    #[test]
    fn gate_request_roundtrips() {
        let request = GateRequest::Launch {
            args: vec!["--join".into(), "address with spaces".into()],
            env: vec![GateEnv {
                name: "RUST_LOG".into(),
                value: "game=debug".into(),
            }],
        };
        let encoded = serde_json::to_vec(&request).unwrap();
        assert_eq!(
            serde_json::from_slice::<GateRequest>(&encoded).unwrap(),
            request
        );
    }

    #[test]
    fn gate_classifies_only_protocol_and_rsync_server_commands() {
        assert_eq!(classify_gate_command(GATE_SENTINEL), GateCommand::Protocol);
        assert_eq!(
            classify_gate_command("rsync --server -logDtpre.iLsfxCIvu . target"),
            GateCommand::Rsync
        );
        for rejected in [
            "sh",
            "sh -lc id",
            "id",
            "FRAGPIPE_SSH_GATE_V1 status",
            "rsync --serverevil",
        ] {
            assert_eq!(classify_gate_command(rejected), GateCommand::Reject);
        }
    }

    #[test]
    fn restricted_targets_must_stay_below_remote_root() {
        let mut remote = test_remote();
        remote.restricted = true;
        assert_eq!(
            restricted_target(&remote, "/remote/assets/game.dat").unwrap(),
            "assets/game.dat"
        );
        assert!(restricted_target(&remote, "/other/game").is_err());
        assert!(restricted_target(&remote, "../outside").is_err());
    }

    #[test]
    fn gate_rejects_traversal_and_non_fixed_paths() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            Gate::new(
                Path::new("relative-root"),
                Path::new("game"),
                Path::new("game.log"),
                Path::new("pid")
            )
            .is_err()
        );
        let root = dir.path().join("root");
        assert!(
            Gate::new(
                &root,
                Path::new("../game"),
                Path::new("game.log"),
                Path::new("pid")
            )
            .is_err()
        );
        assert!(
            Gate::new(
                &root,
                Path::new("bin/game"),
                Path::new("game.log"),
                Path::new("pid")
            )
            .is_err()
        );
    }

    #[test]
    fn gate_rejects_malformed_and_unknown_requests() {
        let dir = tempfile::tempdir().unwrap();
        let gate = Gate::new(
            &dir.path().join("root"),
            Path::new("game"),
            Path::new("game.log"),
            Path::new("pid"),
        )
        .unwrap();
        assert!(
            gate.handle_protocol(Cursor::new(b"not json"), Vec::new())
                .is_err()
        );
        assert!(
            gate.handle_protocol(
                Cursor::new(br#"{"operation":"shell","command":"id"}"#),
                Vec::new()
            )
            .is_err()
        );
        assert!(
            gate.handle_protocol(
                Cursor::new(br#"{"operation":"stop","extra":true}"#),
                Vec::new()
            )
            .is_err()
        );
    }

    #[test]
    fn gate_reads_status_and_clears_fixed_log() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        let gate = Gate::new(
            &root,
            Path::new("game"),
            Path::new("game.log"),
            Path::new("pid"),
        )
        .unwrap();
        fs::write(root.join("game.log"), "first\nsecond\n").unwrap();

        let mut output = Vec::new();
        gate.handle_protocol(
            Cursor::new(serde_json::to_vec(&GateRequest::ReadLog).unwrap()),
            &mut output,
        )
        .unwrap();
        assert_eq!(output, b"first\nsecond\n");

        output.clear();
        gate.handle_protocol(
            Cursor::new(serde_json::to_vec(&GateRequest::Status).unwrap()),
            &mut output,
        )
        .unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&output).unwrap(),
            serde_json::json!({"running": false, "pid": null})
        );

        gate.handle_protocol(
            Cursor::new(serde_json::to_vec(&GateRequest::ClearLog).unwrap()),
            Vec::new(),
        )
        .unwrap();
        assert!(!root.join("game.log").exists());
    }

    #[cfg(unix)]
    #[test]
    fn gate_rejects_log_symlinks_outside_root() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        let outside = dir.path().join("outside.log");
        let gate = Gate::new(
            &root,
            Path::new("game"),
            Path::new("game.log"),
            Path::new("pid"),
        )
        .unwrap();
        fs::write(&outside, "secret").unwrap();
        symlink(&outside, root.join("game.log")).unwrap();
        assert!(gate.read_log().is_err());
        assert!(gate.clear_log().is_err());
        assert_eq!(fs::read_to_string(outside).unwrap(), "secret");
    }

    #[cfg(unix)]
    #[test]
    fn restricted_launch_and_stop_use_fixed_pid_and_log() {
        use std::os::unix::fs::PermissionsExt as _;
        use std::thread;
        use std::time::Duration;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        let gate = Gate::new(
            &root,
            Path::new("game"),
            Path::new("game.log"),
            Path::new("pid"),
        )
        .unwrap();
        let binary = root.join("game");
        fs::write(&binary, "#!/bin/sh\necho started\nexec sleep 30\n").unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();

        gate.launch(&[], &[]).unwrap();
        let process = gate.read_pid().unwrap().unwrap();
        assert!(process.is_running().unwrap());
        assert_eq!(
            gate.status().unwrap(),
            GateStatus {
                running: true,
                pid: Some(process.pid)
            }
        );
        for _ in 0..50 {
            if String::from_utf8_lossy(&gate.read_log().unwrap()).contains("started") {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(String::from_utf8_lossy(&gate.read_log().unwrap()).contains("started"));

        gate.stop().unwrap();
        assert!(!root.join("pid").exists());
        assert_eq!(
            gate.status().unwrap(),
            GateStatus {
                running: false,
                pid: None
            }
        );
    }
}
