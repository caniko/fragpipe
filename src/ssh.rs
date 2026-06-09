use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};

use crate::config::{Config, RemotePeer, binary_file_name, project_root, resolve_path};
use crate::process::ensure_success;
use crate::util::{shell_args, shell_quote};

pub fn deploy(config: &Config, remote: &RemotePeer, dry_run: bool) -> Result<()> {
    let project_root = project_root(config);
    run_ssh(
        &remote.host,
        &format!("mkdir -p {}", shell_quote(&remote.remote_dir)),
        dry_run,
    )?;

    let binary_name = remote_binary_name(config, remote)?;
    let remote_binary = format!("{}/{}", remote.remote_dir, binary_name);
    run_rsync(
        &config.game.binary,
        &remote.host,
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
        run_rsync(assets_dir, &remote.host, &target, project_root, dry_run)?;
    }

    for item in &remote.deploy {
        run_rsync(
            &item.source,
            &remote.host,
            &item.target,
            project_root,
            dry_run,
        )?;
    }

    run_ssh(
        &remote.host,
        &format!("chmod +x {}", shell_quote(&remote_binary)),
        dry_run,
    )
}

pub fn launch_remote(
    config: &Config,
    remote: &RemotePeer,
    args: &[String],
    dry_run: bool,
) -> Result<()> {
    let binary_name = remote_binary_name(config, remote)?;
    let mut env = String::new();
    for pair in &config.game.env {
        env.push_str(&format!("{}={} ", pair.name, shell_quote(&pair.value)));
    }
    for pair in &remote.env {
        env.push_str(&format!("{}={} ", pair.name, shell_quote(&pair.value)));
    }

    let remote_binary = format!("{}/{}", remote.remote_dir, binary_name);
    let command = format!(
        "cd {} && {}nohup {} {} > {} 2>&1 < /dev/null &",
        shell_quote(&remote.remote_dir),
        env,
        shell_quote(&remote_binary),
        shell_args(args),
        shell_quote(&remote.log_file),
    );
    println!("==> Remote joining peer on {}: {}", remote.name, command);
    run_ssh(&remote.host, &command, dry_run)
}

pub fn stop_remote(config: &Config, remote: &RemotePeer, dry_run: bool) -> Result<()> {
    let kill_name = crate::config::kill_name(config)?;
    run_ssh(
        &remote.host,
        &format!("pkill -x {} 2>/dev/null || true", shell_quote(kill_name)),
        dry_run,
    )
}

pub fn remote_log(remote: &RemotePeer) -> Result<String> {
    let command = format!(
        "tail -n 200 {}/{} 2>/dev/null || true",
        shell_quote(&remote.remote_dir),
        shell_quote(&remote.log_file),
    );
    let output = Command::new("ssh")
        .arg(&remote.host)
        .arg(command)
        .output()
        .with_context(|| format!("failed to read remote log from {}", remote.host))?;
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn run_ssh(host: &str, command: &str, dry_run: bool) -> Result<()> {
    println!("ssh {host} {}", shell_quote(command));
    if dry_run {
        return Ok(());
    }
    let status = Command::new("ssh")
        .arg(host)
        .arg(command)
        .status()
        .with_context(|| format!("failed to run ssh command on {host}"))?;
    ensure_success(status, "ssh command")
}

fn run_rsync(
    source: &Path,
    host: &str,
    target: &str,
    project_root: &Path,
    dry_run: bool,
) -> Result<()> {
    let resolved = resolve_path(project_root, source);
    println!("rsync -az {} {host}:{target}", resolved.display());
    if dry_run {
        return Ok(());
    }
    let status = Command::new("rsync")
        .arg("-az")
        .arg(&resolved)
        .arg(format!("{host}:{target}"))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DeployPath;
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
            log_file: "game.log".into(),
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
    fn stop_remote_dry_run_succeeds() {
        let config = test_config();
        let remote = test_remote();
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
}
