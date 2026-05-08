use std::fs::{self, File};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};

use anyhow::{Context, Result, bail};

use crate::config::{Config, EnvPair, project_root};
use crate::util::command_line;

pub fn run_build(config: &Config, dry_run: bool) -> Result<()> {
    let Some(command) = config.game.build_command.as_deref() else {
        return Ok(());
    };
    run_shell_command("Building", command, project_root(config), dry_run)
}

pub fn run_shell_command(label: &str, command: &str, cwd: &Path, dry_run: bool) -> Result<()> {
    println!("==> {label}: {command}");
    if dry_run {
        return Ok(());
    }
    let status = shell_command(command)
        .current_dir(cwd)
        .status()
        .with_context(|| format!("failed to run {label} command"))?;
    ensure_success(status, label)
}

pub fn spawn_logged(
    config: &Config,
    args: &[String],
    log_path: &Path,
    dry_run: bool,
    label: &str,
) -> Result<Child> {
    let command = command_line(&config.game.binary, args);
    println!("==> {label}: {command}");
    if dry_run {
        return spawn_noop_child();
    }

    let log = File::create(log_path)
        .with_context(|| format!("failed to create log {}", log_path.display()))?;
    let log_err = log.try_clone().context("failed to clone log file")?;
    let mut cmd = Command::new(&config.game.binary);
    cmd.args(args)
        .current_dir(project_root(config))
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err));
    apply_env(&mut cmd, &config.game.env);
    cmd.env("BEVY_ASSET_ROOT", project_root(config));
    spawn_process_group(cmd, "failed to launch local process")
}

pub fn kill_child(child: &mut Child) {
    #[cfg(unix)]
    {
        let _ = Command::new("kill")
            .arg("-TERM")
            .arg(format!("-{}", child.id()))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let _ = child.kill();
    let _ = child.wait();
}

pub fn remove_if_exists(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("failed to remove {}", path.display())),
    }
}

pub fn apply_env(cmd: &mut Command, env: &[EnvPair]) {
    for pair in env {
        cmd.env(&pair.name, &pair.value);
    }
}

pub fn ensure_success(status: ExitStatus, label: &str) -> Result<()> {
    if status.success() {
        Ok(())
    } else {
        bail!("{label} failed with status {status}")
    }
}

fn shell_command(command: &str) -> Command {
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(command);
    cmd
}

fn spawn_process_group(mut cmd: Command, context: &'static str) -> Result<Child> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        cmd.process_group(0);
    }
    cmd.spawn().context(context)
}

fn spawn_noop_child() -> Result<Child> {
    Command::new("sh")
        .arg("-c")
        .arg("sleep 0")
        .spawn()
        .context("failed to spawn dry-run placeholder process")
}
