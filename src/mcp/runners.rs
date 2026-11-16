use anyhow::{Context, Result, ensure};
use mcp_layer::{
    jobs::JobSpec,
    profiles::{Runner, RunnerRegistry},
};
use serde::Deserialize;
use toml::Table;

use super::FragpipeCtx;

pub fn register(registry: &mut RunnerRegistry, ctx: &FragpipeCtx) -> Result<()> {
    for name in [
        "direct-1v1",
        "direct-tournament",
        "webrtc-1v1",
        "internet-1v1",
        "android-1v1",
        "android-ui",
        "ship",
    ] {
        registry.register(FragpipeRunner {
            name,
            ctx: ctx.clone(),
        })?;
    }
    Ok(())
}

struct FragpipeRunner {
    name: &'static str,
    ctx: FragpipeCtx,
}

#[derive(Default, Deserialize)]
struct Common {
    config: Option<String>,
    max_runs: Option<u32>,
    timeout: Option<u64>,
    stop_on_failure: Option<bool>,
    no_build: Option<bool>,
    dry_run: Option<bool>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Direct {
    #[serde(flatten)]
    common: Common,
    remote: String,
    transport: Option<String>,
    port: Option<u16>,
    local_ip: Option<String>,
    no_deploy: Option<bool>,
    headless: Option<bool>,
    windowed: Option<bool>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Tournament {
    #[serde(flatten)]
    common: Common,
    remote: String,
    local_ip: Option<String>,
    no_deploy: Option<bool>,
    windowed: Option<bool>,
    host_extra_args: Option<Vec<String>>,
    joiner_extra_args: Option<Vec<String>>,
    drop_args: Option<Vec<String>>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Webrtc {
    #[serde(flatten)]
    common: Common,
    remote: Option<String>,
    local_ip: Option<String>,
    webrtc_port: Option<u16>,
    no_deploy: Option<bool>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Internet {
    #[serde(flatten)]
    common: Common,
    smoke_workdir: Option<String>,
    game_bin: Option<String>,
    rdv_bin: Option<String>,
    asset_root: Option<String>,
    log_dir: Option<String>,
    pass_marker: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Android {
    #[serde(flatten)]
    common: Common,
    no_install: Option<bool>,
    device: Option<bool>,
    adb_serial: Option<String>,
    slot: Option<String>,
    workers_config: Option<String>,
    launch_config: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Ship {
    #[serde(flatten)]
    common: Common,
    remote: String,
    no_deploy: Option<bool>,
    require_green: Option<bool>,
    restart: Option<bool>,
    launch_args: Option<Vec<String>>,
}
fn option<T: ToString>(args: &mut Vec<String>, name: &str, value: Option<T>) {
    if let Some(value) = value {
        args.push(format!("--{name}={}", value.to_string()));
    }
}
fn flag(args: &mut Vec<String>, name: &str, value: Option<bool>) {
    if value == Some(true) {
        args.push(format!("--{name}"));
    }
}
fn common(spec: &mut JobSpec, p: &Common, timeout: u64, internet: bool, ship: bool) {
    option(&mut spec.argv, "config", p.config.as_deref());
    if !ship {
        let runs = p.max_runs.unwrap_or(1).min(50);
        let timeout = p.timeout.unwrap_or(timeout);
        option(&mut spec.argv, "max-runs", Some(runs));
        option(
            &mut spec.argv,
            if internet { "timeout-secs" } else { "timeout" },
            Some(timeout),
        );
        if p.stop_on_failure == Some(false) {
            spec.argv.push("--stop-on-failure=false".into());
        }
        spec.timeout_secs = timeout.saturating_mul(u64::from(runs)).saturating_add(1200);
    } else {
        spec.timeout_secs = p.timeout.unwrap_or(timeout);
    }
    if !internet {
        flag(&mut spec.argv, "no-build", p.no_build);
    }
    flag(&mut spec.argv, "dry-run", p.dry_run);
}
fn extra(args: &mut Vec<String>, name: &str, values: Option<Vec<String>>) -> Result<()> {
    if let Some(values) = values {
        ensure!(
            values
                .iter()
                .all(|s| !s.is_empty() && !s.chars().any(char::is_whitespace)),
            "{name} elements must not contain whitespace or be empty"
        );
        if !values.is_empty() {
            option(args, name, Some(values.join(" ")));
        }
    }
    Ok(())
}
impl FragpipeRunner {
    /// `direct-stop` for the remote on cancel; none for a dry run, which started nothing.
    fn cleanup(&self, common: &Common, remote: &str, tournament: bool) -> Option<Vec<String>> {
        if common.dry_run == Some(true) {
            return None;
        }
        let mut cleanup = self.ctx.fragpipe_argv.clone();
        cleanup.extend([
            "direct-stop".into(),
            format!(
                "--config={}",
                common.config.as_deref().unwrap_or("fragpipe.toml")
            ),
            format!("--remote={remote}"),
        ]);
        if tournament {
            cleanup.push("--tournament".into());
        }
        Some(cleanup)
    }
    fn windowed(&self, common: &Common, remote: &str) -> Result<()> {
        let cfg = self.config(common)?;
        let peer = cfg
            .get("remote")
            .and_then(toml::Value::as_array)
            .and_then(|peers| {
                peers
                    .iter()
                    .find(|p| p.get("name").and_then(toml::Value::as_str) == Some(remote))
            });
        ensure!(
            peer.and_then(|p| p.get("session_user"))
                .and_then(toml::Value::as_str)
                .is_some_and(|s| !s.trim().is_empty()),
            "windowed remote '{remote}' requires session_user"
        );
        ensure!(
            ["DISPLAY", "WAYLAND_DISPLAY"]
                .iter()
                .any(|key| std::env::var_os(key).is_some_and(|v| !v.is_empty())),
            "windowed run requires a local display"
        );
        Ok(())
    }
    fn config(&self, common: &Common) -> Result<Table> {
        let path = self
            .ctx
            .project_root
            .join(common.config.as_deref().unwrap_or("fragpipe.toml"));
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        Ok(toml::from_str(&text)?)
    }
    fn direct(
        &self,
        spec: &mut JobSpec,
        common: &Common,
        remote: &str,
        port: Option<u16>,
        tournament: bool,
    ) -> Result<()> {
        let cfg = self.config(common)?;
        let section = if tournament { "tournament" } else { "direct" };
        let table = cfg.get(section).and_then(toml::Value::as_table);
        let configured_port = table
            .and_then(|t| t.get("port"))
            .and_then(toml::Value::as_integer);
        let port = match (port, configured_port) {
            (Some(port), _) => port,
            (None, Some(port)) => u16::try_from(port).context("invalid LAN port")?,
            (None, None) if !tournament => crate::config::DirectConfig::default().port,
            _ => anyhow::bail!("tournament port missing"),
        };
        spec.locks.push(format!("lan-udp-{port}"));
        spec.cleanup = self.cleanup(common, remote, tournament);
        if let Some(markers) = cfg
            .get("process")
            .and_then(|v| v.get("fatal_markers"))
            .and_then(toml::Value::as_array)
        {
            spec.fail_markers = markers
                .iter()
                .filter_map(toml::Value::as_str)
                .map(regex::escape)
                .collect();
        }
        {
            let dir = table
                .and_then(|t| t.get("artifact_dir"))
                .and_then(toml::Value::as_str)
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| {
                    if tournament {
                        crate::config::default_tournament_artifact_dir()
                    } else {
                        crate::config::default_direct_artifact_dir()
                    }
                });
            let root = cfg
                .get("game")
                .and_then(|g| g.get("project_root"))
                .and_then(toml::Value::as_str)
                .unwrap_or(".");
            let dir = std::path::Path::new(root).join(dir);
            for run in 1..=common.max_runs.unwrap_or(1).min(50) {
                let run_dir = dir.join(format!("run-{run:02}"));
                let mut names = vec!["report.json".to_owned()];
                if tournament {
                    let count = ["local_players", "remote_players"]
                        .iter()
                        .filter_map(|key| {
                            table
                                .and_then(|t| t.get(*key))
                                .and_then(toml::Value::as_integer)
                        })
                        .sum::<i64>();
                    for index in 0..count.min(8) {
                        names.push(format!("peer-{index}.log"));
                    }
                } else {
                    names.extend(["listener.log".into(), "remote.log".into()]);
                }
                spec.artifacts.extend(
                    names
                        .into_iter()
                        .map(|name| run_dir.join(name).to_string_lossy().into_owned()),
                );
            }
        }
        Ok(())
    }
}
impl Runner for FragpipeRunner {
    fn name(&self) -> &str {
        self.name
    }
    fn build(&self, params: &toml1::Table) -> Result<JobSpec> {
        ensure!(!self.ctx.fragpipe_argv.is_empty(), "empty fragpipe argv");
        let mut spec = JobSpec {
            runner: self.name.into(),
            cwd: self.ctx.project_root.clone(),
            argv: self.ctx.fragpipe_argv.clone(),
            ..Default::default()
        };
        spec.argv.push(self.name.into());
        match self.name {
            "direct-1v1" => {
                let p: Direct = params.clone().try_into()?;
                if p.windowed == Some(true) {
                    ensure!(p.headless != Some(true), "windowed and headless conflict");
                    self.windowed(&p.common, &p.remote)?;
                }
                common(&mut spec, &p.common, 300, false, false);
                option(&mut spec.argv, "remote", Some(&p.remote));
                option(&mut spec.argv, "transport", p.transport);
                option(&mut spec.argv, "port", p.port);
                option(&mut spec.argv, "local-ip", p.local_ip);
                flag(&mut spec.argv, "no-deploy", p.no_deploy);
                flag(&mut spec.argv, "headless", p.headless);
                self.direct(&mut spec, &p.common, &p.remote, p.port, false)?;
            }
            "direct-tournament" => {
                let p: Tournament = params.clone().try_into()?;
                if p.windowed == Some(true) {
                    self.windowed(&p.common, &p.remote)?;
                }
                common(&mut spec, &p.common, 600, false, false);
                option(&mut spec.argv, "remote", Some(&p.remote));
                option(&mut spec.argv, "local-ip", p.local_ip);
                flag(&mut spec.argv, "no-deploy", p.no_deploy);
                extra(&mut spec.argv, "host-extra-args", p.host_extra_args)?;
                extra(&mut spec.argv, "joiner-extra-args", p.joiner_extra_args)?;
                let mut drop_args = p.drop_args.unwrap_or_default();
                if p.windowed == Some(true) {
                    drop_args.push("--headless".into());
                }
                extra(&mut spec.argv, "drop-args", Some(drop_args))?;
                self.direct(&mut spec, &p.common, &p.remote, None, true)?;
            }
            "webrtc-1v1" => {
                let p: Webrtc = params.clone().try_into()?;
                if let Some(remote) = &p.remote {
                    spec.cleanup = self.cleanup(&p.common, remote, false);
                }
                common(&mut spec, &p.common, 300, false, false);
                option(&mut spec.argv, "remote", p.remote);
                option(&mut spec.argv, "local-ip", p.local_ip);
                option(&mut spec.argv, "webrtc-port", p.webrtc_port);
                flag(&mut spec.argv, "no-deploy", p.no_deploy);
            }
            "internet-1v1" => {
                let p: Internet = params.clone().try_into()?;
                common(&mut spec, &p.common, 200, true, false);
                option(&mut spec.argv, "workdir", p.smoke_workdir);
                option(&mut spec.argv, "game-bin", p.game_bin);
                option(&mut spec.argv, "rdv-bin", p.rdv_bin);
                option(&mut spec.argv, "asset-root", p.asset_root);
                option(&mut spec.argv, "log-dir", p.log_dir);
                option(&mut spec.argv, "pass-marker", p.pass_marker);
            }
            "android-1v1" | "android-ui" => {
                let p: Android = params.clone().try_into()?;
                ensure!(
                    self.name == "android-ui" || p.launch_config.is_none(),
                    "launch_config is only supported by android-ui"
                );
                common(
                    &mut spec,
                    &p.common,
                    if self.name == "android-ui" { 120 } else { 300 },
                    false,
                    false,
                );
                let cfg = self.config(&p.common)?;
                // Lock on the device serial: a slot name resolves to its serial through
                // the workers config, so a slot and its serial exclude each other.
                let serial = match (&p.slot, &p.adb_serial) {
                    (Some(slot), _) => {
                        let path = p
                            .workers_config
                            .as_ref()
                            .map(|path| self.ctx.project_root.join(path));
                        let workers = crate::config::load_worker_config(path.as_deref())?;
                        let (_, slots) = crate::config::select_android_slots(
                            &workers,
                            std::slice::from_ref(slot),
                        )?;
                        slots[0].adb_serial.clone()
                    }
                    (None, Some(serial)) => serial.clone(),
                    (None, None) => cfg
                        .get("android")
                        .and_then(|android| android.get("adb_serial"))
                        .and_then(toml::Value::as_str)
                        .unwrap_or("default")
                        .to_owned(),
                };
                flag(&mut spec.argv, "no-install", p.no_install);
                flag(&mut spec.argv, "device", p.device);
                option(&mut spec.argv, "adb-serial", p.adb_serial.as_deref());
                option(&mut spec.argv, "slot", p.slot.as_deref());
                option(&mut spec.argv, "workers-config", p.workers_config);
                option(&mut spec.argv, "launch-config", p.launch_config);
                let serial: String = serial
                    .bytes()
                    .map(|b| {
                        if b.is_ascii_alphanumeric() || b"-_.".contains(&b) {
                            (b as char).to_string()
                        } else {
                            format!("_{b:02x}")
                        }
                    })
                    .collect();
                spec.locks.push(format!("android-{serial}"));
            }
            "ship" => {
                let p: Ship = params.clone().try_into()?;
                // Only a restarting ship launches a remote game this job owns; a plain
                // deploy must not stop a game that was already running there.
                if p.restart == Some(true) {
                    spec.cleanup = self.cleanup(&p.common, &p.remote, false);
                }
                common(&mut spec, &p.common, 600, false, true);
                option(&mut spec.argv, "remote", Some(p.remote));
                flag(&mut spec.argv, "no-deploy", p.no_deploy);
                flag(&mut spec.argv, "require-green", p.require_green);
                flag(&mut spec.argv, "restart", p.restart);
                if let Some(args) = p.launch_args
                    && !args.is_empty()
                {
                    spec.argv.push("--launch-args".into());
                    spec.argv.extend(args);
                }
            }
            _ => unreachable!(),
        }
        Ok(spec)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn android_slot_and_its_serial_take_the_same_lock() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("fragpipe.toml"), "").unwrap();
        std::fs::write(
            dir.path().join("workers.toml"),
            r#"
[[android_worker]]
name = "local"
host = "fake-host"
local_port = 15037

[[android_worker.slots]]
name = "pixel"
kind = "device"
adb_serial = "SERIAL1"
"#,
        )
        .unwrap();
        let ctx = FragpipeCtx {
            project_root: dir.path().into(),
            fragpipe_argv: vec!["fragpipe".into()],
        };
        let runner = FragpipeRunner {
            name: "android-1v1",
            ctx,
        };
        let locks = |params: &str| {
            runner
                .build(&toml1::from_str(params).unwrap())
                .unwrap()
                .locks
        };
        let by_slot = locks("slot = \"pixel\"\nworkers_config = \"workers.toml\"");
        let by_serial = locks("adb_serial = \"SERIAL1\"");
        for params in [
            "slot = \"pixel\"\nadb_serial = \"OTHER\"\nworkers_config = \"workers.toml\"",
            "adb_serial = \"OTHER\"\nslot = \"pixel\"\nworkers_config = \"workers.toml\"",
        ] {
            assert_eq!(locks(params), by_slot);
        }
        assert_eq!(by_slot, by_serial);
        assert!(
            by_slot.contains(&"android-SERIAL1".to_string()),
            "{by_slot:?}"
        );
        assert!(
            runner
                .build(
                    &toml1::from_str("slot = \"missing\"\nworkers_config = \"workers.toml\"")
                        .unwrap()
                )
                .is_err()
        );
    }

    #[test]
    fn android_1v1_rejects_launch_config() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("fragpipe.toml"), "").unwrap();
        let runner = FragpipeRunner {
            name: "android-1v1",
            ctx: FragpipeCtx {
                project_root: dir.path().into(),
                fragpipe_argv: vec!["fragpipe".into()],
            },
        };
        assert!(
            runner
                .build(&toml1::from_str("launch_config = '{}' ").unwrap())
                .is_err()
        );
    }

    #[test]
    fn remote_webrtc_has_cleanup() {
        let dir = tempfile::tempdir().unwrap();
        let runner = FragpipeRunner {
            name: "webrtc-1v1",
            ctx: FragpipeCtx {
                project_root: dir.path().into(),
                fragpipe_argv: vec!["fragpipe".into()],
            },
        };
        let spec = runner
            .build(&toml1::from_str("remote = 'nomad'").unwrap())
            .unwrap();
        assert_eq!(
            spec.cleanup.unwrap(),
            [
                "fragpipe",
                "direct-stop",
                "--config=fragpipe.toml",
                "--remote=nomad"
            ]
        );
    }

    #[test]
    fn only_a_restarting_ship_stops_the_remote_on_cancel() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = FragpipeCtx {
            project_root: dir.path().into(),
            fragpipe_argv: vec!["fragpipe".into()],
        };
        let runner = FragpipeRunner { name: "ship", ctx };
        let deploy = runner
            .build(&toml1::from_str("remote = 'nomad'").unwrap())
            .unwrap();
        assert!(deploy.cleanup.is_none());
        let restart = runner
            .build(&toml1::from_str("remote = 'nomad'\nrestart = true").unwrap())
            .unwrap();
        assert_eq!(restart.cleanup.unwrap()[1], "direct-stop");
        // A dry run started nothing remote, so cancelling it must not stop anything.
        let dry = runner
            .build(&toml1::from_str("remote = 'nomad'\nrestart = true\ndry_run = true").unwrap())
            .unwrap();
        assert!(dry.cleanup.is_none());
        let webrtc = FragpipeRunner {
            name: "webrtc-1v1",
            ctx: FragpipeCtx {
                project_root: dir.path().into(),
                fragpipe_argv: vec!["fragpipe".into()],
            },
        };
        let dry = webrtc
            .build(&toml1::from_str("remote = 'nomad'\ndry_run = true").unwrap())
            .unwrap();
        assert!(dry.cleanup.is_none());
    }

    #[test]
    fn default_artifacts_are_declared() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("fragpipe.toml"),
            "[tournament]\nport=27100\nlocal_players=1\nremote_players=1\n",
        )
        .unwrap();
        for name in ["direct-1v1", "direct-tournament"] {
            let runner = FragpipeRunner {
                name,
                ctx: FragpipeCtx {
                    project_root: dir.path().into(),
                    fragpipe_argv: vec!["fragpipe".into()],
                },
            };
            let spec = runner
                .build(&toml1::from_str("remote = 'nomad'").unwrap())
                .unwrap();
            assert!(
                spec.artifacts
                    .iter()
                    .any(|p| p.ends_with(&format!("{name}/run-01/report.json"))),
                "{name}: {:?}",
                spec.artifacts
            );
        }
    }

    #[test]
    fn windowed_requires_selected_remote_session_user() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("fragpipe.toml"), "[tournament]\nport=27100\n[[remote]]\nname='nomad'\n[[remote]]\nname='other'\nsession_user='tester'\n").unwrap();
        for name in ["direct-1v1", "direct-tournament"] {
            let runner = FragpipeRunner {
                name,
                ctx: FragpipeCtx {
                    project_root: dir.path().into(),
                    fragpipe_argv: vec!["fragpipe".into()],
                },
            };
            let err = runner
                .build(&toml1::from_str("remote='nomad'\nwindowed=true").unwrap())
                .unwrap_err();
            assert!(err.to_string().contains("session_user"), "{err}");
        }
    }

    #[test]
    fn every_runner_builds_parseable_cli_with_argv_prefix() {
        use clap::Parser;
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("fragpipe.toml"),
            "[tournament]\nport=27100\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("workers.toml"), "[[android_worker]]\nname='worker'\nhost='host'\nlocal_port=15037\n[[android_worker.slots]]\nname='pixel'\nkind='device'\nadb_serial='SERIAL1'\n").unwrap();
        let cases = [
            (
                "direct-1v1",
                "remote='nomad'\ntransport='lan'\nport=27100\nlocal_ip='127.0.0.1'\nno_deploy=true\nheadless=true",
            ),
            (
                "direct-tournament",
                "remote='nomad'\nlocal_ip='127.0.0.1'\nno_deploy=true\nwindowed=false\nhost_extra_args=['--host']\njoiner_extra_args=['--join']\ndrop_args=['--headless']",
            ),
            (
                "webrtc-1v1",
                "remote='nomad'\nlocal_ip='127.0.0.1'\nwebrtc_port=27200\nno_deploy=true",
            ),
            (
                "internet-1v1",
                "smoke_workdir='.'\ngame_bin='game'\nrdv_bin='rdv'\nasset_root='assets'\nlog_dir='logs'\npass_marker='PASS'",
            ),
            (
                "android-1v1",
                "no_install=true\ndevice=true\nadb_serial='OTHER'\nslot='pixel'\nworkers_config='workers.toml'",
            ),
            (
                "android-ui",
                "no_install=true\ndevice=true\nadb_serial='OTHER'\nslot='pixel'\nworkers_config='workers.toml'\nlaunch_config='{}'",
            ),
            (
                "ship",
                "remote='nomad'\nno_deploy=true\nrequire_green=true\nrestart=true\nlaunch_args=['--headless']",
            ),
        ];
        for (name, params) in cases {
            let runner = FragpipeRunner {
                name,
                ctx: FragpipeCtx {
                    project_root: dir.path().into(),
                    fragpipe_argv: vec!["cb".into(), "gameplay".into()],
                },
            };
            let common = if name == "internet-1v1" {
                "config='fragpipe.toml'\nmax_runs=2\ntimeout=30\nstop_on_failure=false\ndry_run=true\n"
            } else {
                "config='fragpipe.toml'\nmax_runs=2\ntimeout=30\nstop_on_failure=false\nno_build=true\ndry_run=true\n"
            };
            let spec = runner
                .build(&toml1::from_str(&format!("{common}{params}")).unwrap())
                .unwrap();
            assert_eq!(&spec.argv[..3], ["cb", "gameplay", name]);
            let argv = std::iter::once("fragpipe").chain(spec.argv[2..].iter().map(String::as_str));
            crate::Cli::try_parse_from(argv).unwrap_or_else(|e| panic!("{name}: {e}"));
            if let Some(cleanup) = spec.cleanup {
                assert_eq!(&cleanup[..2], ["cb", "gameplay"]);
            }
        }
    }
}
