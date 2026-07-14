use std::fs::{self, File};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};

#[cfg(unix)]
use std::sync::{
    Arc, OnceLock,
    atomic::{AtomicBool, Ordering},
};

use anyhow::{Context, Result, bail};

use crate::config::{Config, EnvPair, project_root};
use crate::util::command_line;

#[cfg(unix)]
static TERMINATION_REQUESTED: OnceLock<Arc<AtomicBool>> = OnceLock::new();
#[cfg(unix)]
static TERMINATION_HANDLERS: OnceLock<std::result::Result<(), String>> = OnceLock::new();

/// Install cancellation handlers for long-running fix-loop commands.
///
/// The handlers only set an atomic flag. Polling loops observe it and return
/// normally so their existing local, remote, and Android teardown paths run.
pub fn install_termination_handlers() -> Result<()> {
    #[cfg(unix)]
    {
        let flag = TERMINATION_REQUESTED
            .get_or_init(|| Arc::new(AtomicBool::new(false)))
            .clone();
        flag.store(false, Ordering::SeqCst);
        let installation = TERMINATION_HANDLERS.get_or_init(|| {
            signal_hook::flag::register(signal_hook::consts::SIGINT, flag.clone())
                .map_err(|error| error.to_string())?;
            signal_hook::flag::register(signal_hook::consts::SIGTERM, flag)
                .map_err(|error| error.to_string())?;
            Ok(())
        });
        if let Err(error) = installation {
            bail!("failed to install Fragpipe termination handlers: {error}");
        }
    }
    Ok(())
}

/// Return an error after SIGINT/SIGTERM so callers unwind through teardown.
pub fn check_interrupted() -> Result<()> {
    #[cfg(unix)]
    if TERMINATION_REQUESTED
        .get()
        .is_some_and(|flag| flag.load(Ordering::SeqCst))
    {
        bail!("fix-loop interrupted by termination signal");
    }
    Ok(())
}

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
    label: &str,
) -> Result<Child> {
    let command = command_line(&config.game.binary, args);
    println!("==> {label}: {command}");

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

pub(crate) fn spawn_process_group(mut cmd: Command, context: &'static str) -> Result<Child> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        cmd.process_group(0);

        // Game and Android helper processes live in their own groups so timeout
        // cleanup can terminate their full process trees. On Linux, also ask
        // the kernel to terminate the group leader if Fragpipe itself exits
        // abruptly (SIGINT/SIGTERM, MCP timeout, or parent crash). Otherwise an
        // interrupted fix-loop leaves peers holding ports for the next run.
        #[cfg(target_os = "linux")]
        {
            let parent_pid = std::process::id() as libc::pid_t;
            // SAFETY: `pre_exec` only invokes async-signal-safe libc operations
            // between fork and exec. The captured value is a plain integer.
            unsafe {
                cmd.pre_exec(move || {
                    if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                    // Close the fork-to-prctl race: if the parent already died,
                    // terminate before exec rather than becoming an orphan.
                    if libc::getppid() != parent_pid {
                        libc::raise(libc::SIGTERM);
                    }
                    Ok(())
                });
            }
        }
    }
    cmd.spawn().context(context)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ensure_success_returns_ok_for_success_status() {
        let status = Command::new("true").status().unwrap();
        assert!(ensure_success(status, "true").is_ok());
    }

    #[test]
    fn ensure_success_errors_for_failure_status() {
        let status = Command::new("false").status().unwrap();
        let result = ensure_success(status, "false");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("false"));
    }

    #[test]
    fn remove_if_exists_removes_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, "content").unwrap();
        remove_if_exists(&path).unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn remove_if_exists_ok_for_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nonexistent.txt");
        remove_if_exists(&path).unwrap();
    }

    #[test]
    fn apply_env_sets_environment_variables() {
        let env = vec![EnvPair {
            name: "FRAGPIPE_TEST_KEY".into(),
            value: "test_val".into(),
        }];
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("echo $FRAGPIPE_TEST_KEY");
        apply_env(&mut cmd, &env);
        let output = cmd.output().unwrap();
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "test_val");
    }

    #[test]
    fn apply_env_multiple_pairs() {
        let env = vec![
            EnvPair {
                name: "KEY_A".into(),
                value: "VAL_A".into(),
            },
            EnvPair {
                name: "KEY_B".into(),
                value: "VAL_B".into(),
            },
        ];
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("echo $KEY_A-$KEY_B");
        apply_env(&mut cmd, &env);
        let output = cmd.output().unwrap();
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            "VAL_A-VAL_B"
        );
    }

    #[test]
    fn kill_child_terminates_running_process() {
        let mut child = Command::new("sh")
            .arg("-c")
            .arg("sleep 30")
            .spawn()
            .unwrap();
        // Should not panic
        kill_child(&mut child);
        // Process should be gone
        assert!(child.try_wait().unwrap().is_some());
    }
}
